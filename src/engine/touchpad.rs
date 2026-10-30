//! Raw finger positions from Windows precision touchpads.
//!
//! Precision touchpads report a HID digitizer (usage page 0x0D, usage 0x05).
//! Each report carries some contacts plus a contact count; in "hybrid" mode a
//! frame with more fingers than fit in one report is split across reports,
//! and only the first report has a non-zero contact count.

use std::collections::HashMap;

use windows::Win32::Devices::HumanInterfaceDevice::*;
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::UI::Input::*;

const PAGE_DIGITIZER: u16 = 0x0D;
const PAGE_GENERIC: u16 = 0x01;
const USAGE_TOUCHPAD: u16 = 0x05;
const USAGE_TIP: u16 = 0x42;
const USAGE_CONTACT_ID: u16 = 0x51;
const USAGE_CONTACT_COUNT: u16 = 0x54;
const USAGE_X: u16 = 0x30;
const USAGE_Y: u16 = 0x31;
const HIDP_OK: i32 = 0x0011_0000;

/// One finger on the pad, in "pixel-ish" units (scaled so a full swipe
/// across the pad is roughly one screen width).
#[derive(Clone, Copy, Debug)]
pub struct Contact {
    /// Stays the same while the finger is down (the report slot when the
    /// device doesn't send contact ids).
    pub id: u32,
    pub x: f64,
    pub y: f64,
}

struct Device {
    preparsed: Vec<u8>,
    /// Link collections that hold one contact each, in report order.
    contact_lcs: Vec<u16>,
    count_lc: u16,
    sx: f64,
    sy: f64,
    expected: usize,
    pending: Vec<(bool, Contact)>,
}

#[derive(Default)]
pub struct Touchpads {
    devices: HashMap<isize, Option<Device>>,
}

pub fn register(hwnd: HWND) -> windows::core::Result<()> {
    let dev = RAWINPUTDEVICE {
        usUsagePage: PAGE_DIGITIZER,
        usUsage: USAGE_TOUCHPAD,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    };
    unsafe { RegisterRawInputDevices(&[dev], std::mem::size_of::<RAWINPUTDEVICE>() as u32) }
}

impl Touchpads {
    /// Handles one WM_INPUT. Returns the fingers currently down whenever a
    /// full frame has been assembled.
    pub fn on_input(&mut self, lparam: isize, screen_w: f64) -> Option<Vec<Contact>> {
        let buf = read_raw(HRAWINPUT(lparam as *mut _))?;
        let raw = unsafe { &*(buf.as_ptr() as *const RAWINPUT) };
        if raw.header.dwType != RIM_TYPEHID.0 {
            return None;
        }
        let key = raw.header.hDevice.0 as isize;
        let dev = self
            .devices
            .entry(key)
            .or_insert_with(|| Device::open(raw.header.hDevice, screen_w))
            .as_mut()?;

        let hid = unsafe { &raw.data.hid };
        let size = hid.dwSizeHid as usize;
        let data = unsafe { std::slice::from_raw_parts(hid.bRawData.as_ptr(), size * hid.dwCount as usize) };
        let mut frame = None;
        for report in data.chunks_exact(size) {
            if let Some(f) = dev.on_report(&mut report.to_vec()) {
                frame = Some(f);
            }
        }
        frame
    }
}

fn read_raw(h: HRAWINPUT) -> Option<Vec<u64>> {
    let header = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    let mut size = 0u32;
    unsafe {
        GetRawInputData(h, RID_INPUT, None, &mut size, header);
        // u64 backing keeps the RAWINPUT cast aligned.
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        let got = GetRawInputData(h, RID_INPUT, Some(buf.as_mut_ptr() as *mut _), &mut size, header);
        (got != u32::MAX && got != 0).then_some(buf)
    }
}

impl Device {
    fn open(handle: HANDLE, screen_w: f64) -> Option<Device> {
        let mut size = 0u32;
        unsafe {
            GetRawInputDeviceInfoW(Some(handle), RIDI_PREPARSEDDATA, None, &mut size);
            let mut preparsed = vec![0u8; size as usize];
            let got = GetRawInputDeviceInfoW(
                Some(handle),
                RIDI_PREPARSEDDATA,
                Some(preparsed.as_mut_ptr() as *mut _),
                &mut size,
            );
            if got == u32::MAX || got == 0 {
                return None;
            }
            let pp = PHIDP_PREPARSED_DATA(preparsed.as_ptr() as isize);
            let mut caps = HIDP_CAPS::default();
            if HidP_GetCaps(pp, &mut caps).0 != HIDP_OK {
                return None;
            }
            let mut n = caps.NumberInputValueCaps;
            let mut vcaps = vec![HIDP_VALUE_CAPS::default(); n as usize];
            if HidP_GetValueCaps(HidP_Input, vcaps.as_mut_ptr(), &mut n, pp).0 != HIDP_OK {
                return None;
            }
            let usage = |c: &HIDP_VALUE_CAPS| {
                if c.IsRange { c.Anonymous.Range.UsageMin } else { c.Anonymous.NotRange.Usage }
            };
            let mut contact_lcs = Vec::new();
            let mut count_lc = 0;
            let mut xr = None;
            let mut yr = None;
            for c in &vcaps[..n as usize] {
                match (c.UsagePage, usage(c)) {
                    (PAGE_GENERIC, USAGE_X) => {
                        contact_lcs.push(c.LinkCollection);
                        xr.get_or_insert(c);
                    }
                    (PAGE_GENERIC, USAGE_Y) => {
                        yr.get_or_insert(c);
                    }
                    (PAGE_DIGITIZER, USAGE_CONTACT_COUNT) => count_lc = c.LinkCollection,
                    _ => {}
                }
            }
            let (xr, yr) = (xr?, yr?);
            contact_lcs.sort_unstable();
            contact_lcs.dedup();

            // A full-width swipe across the pad ~= one screen width; y uses the
            // same physical scale so diagonal motion isn't skewed.
            let xspan = (xr.LogicalMax - xr.LogicalMin).max(1) as f64;
            let yspan = (yr.LogicalMax - yr.LogicalMin).max(1) as f64;
            let sx = screen_w / xspan;
            let xphys = (xr.PhysicalMax - xr.PhysicalMin) as f64;
            let yphys = (yr.PhysicalMax - yr.PhysicalMin) as f64;
            let sy = if xphys > 0.0 && yphys > 0.0 {
                sx * (xspan / xphys) / (yspan / yphys)
            } else {
                sx
            };
            log!("touchpad: {} contact slots, x span {xspan}, y span {yspan}", contact_lcs.len());
            Some(Device { preparsed, contact_lcs, count_lc, sx, sy, expected: 0, pending: Vec::new() })
        }
    }

    fn on_report(&mut self, report: &mut [u8]) -> Option<Vec<Contact>> {
        let pp = PHIDP_PREPARSED_DATA(self.preparsed.as_ptr() as isize);
        let count = value(pp, PAGE_DIGITIZER, self.count_lc, USAGE_CONTACT_COUNT, report).unwrap_or(0) as usize;
        if count > 0 {
            self.expected = count;
            self.pending.clear();
        }
        if self.expected == 0 {
            return None;
        }
        for &lc in &self.contact_lcs {
            if self.pending.len() >= self.expected {
                break;
            }
            let (Some(x), Some(y)) = (
                value(pp, PAGE_GENERIC, lc, USAGE_X, report),
                value(pp, PAGE_GENERIC, lc, USAGE_Y, report),
            ) else {
                continue;
            };
            let id = value(pp, PAGE_DIGITIZER, lc, USAGE_CONTACT_ID, report).unwrap_or(self.pending.len() as u32);
            let tip = tip_down(pp, lc, report);
            self.pending.push((tip, Contact { id, x: x as f64 * self.sx, y: y as f64 * self.sy }));
        }
        if self.pending.len() < self.expected {
            return None;
        }
        self.expected = 0;
        Some(self.pending.drain(..).filter(|(tip, _)| *tip).map(|(_, c)| c).collect())
    }
}

fn value(pp: PHIDP_PREPARSED_DATA, page: u16, lc: u16, usage: u16, report: &mut [u8]) -> Option<u32> {
    let mut v = 0u32;
    let st = unsafe { HidP_GetUsageValue(HidP_Input, page, Some(lc), usage, &mut v, pp, report) };
    (st.0 == HIDP_OK).then_some(v)
}

fn tip_down(pp: PHIDP_PREPARSED_DATA, lc: u16, report: &mut [u8]) -> bool {
    let mut usages = [0u16; 16];
    let mut n = usages.len() as u32;
    let st = unsafe { HidP_GetUsages(HidP_Input, PAGE_DIGITIZER, Some(lc), usages.as_mut_ptr(), &mut n, pp, report) };
    st.0 == HIDP_OK && usages[..n as usize].contains(&USAGE_TIP)
}
