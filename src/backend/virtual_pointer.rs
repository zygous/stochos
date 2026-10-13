//! Synthetic pointer for the Wayland backend.
//!
//! `zwlr_virtual_pointer_v1` is used when the compositor offers it. Compositors
//! without it (COSMIC) fall back to a `/dev/uinput` absolute pointer, which
//! needs write access to that node. Both expose the same small method set, so
//! call sites don't care which one they drive.

use std::cell::{Cell, RefCell};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::time::Duration;

use anyhow::{Context, Result};
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

pub enum VirtualPointer {
    Wlr(ZwlrVirtualPointerV1),
    Uinput(UinputPointer),
}

impl VirtualPointer {
    pub fn motion_absolute(&self, time: u32, x: u32, y: u32, x_extent: u32, y_extent: u32) {
        match self {
            Self::Wlr(vp) => vp.motion_absolute(time, x, y, x_extent, y_extent),
            Self::Uinput(vp) => vp.motion_absolute(x, y, x_extent, y_extent),
        }
    }

    pub fn button(&self, time: u32, button: u32, state: ButtonState) {
        match self {
            Self::Wlr(vp) => vp.button(time, button, state),
            Self::Uinput(vp) => vp.button(button, state == ButtonState::Pressed),
        }
    }

    pub fn axis_source(&self, source: AxisSource) {
        if let Self::Wlr(vp) = self {
            vp.axis_source(source);
        }
    }

    pub fn axis_discrete(&self, time: u32, axis: Axis, value: f64, discrete: i32) {
        match self {
            Self::Wlr(vp) => vp.axis_discrete(time, axis, value, discrete),
            Self::Uinput(vp) => vp.scroll(axis, discrete),
        }
    }

    pub fn frame(&self) {
        match self {
            Self::Wlr(vp) => vp.frame(),
            Self::Uinput(vp) => vp.frame(),
        }
    }
}

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const INPUT_PROP_POINTER: i32 = 0;
const ABS_CNT: usize = 64;
const ABS_RANGE: i32 = 0xffff;

/// Time to let the compositor and the window under the pointer react to a
/// move before the next event. A uinput write is asynchronous, unlike a
/// Wayland roundtrip, and a window that has just regained focus (the overlay
/// was closed to click through it) handles pointer events late: with a shorter
/// wait the first press is often lost while the release still arrives.
const MOTION_SETTLE: Duration = Duration::from_millis(150);

/// Time between button edges. Kept short on purpose: held much longer, a
/// double-click's second release is dropped (seen with Firefox on COSMIC when
/// the second release lands past roughly 400 ms from the first press).
const EDGE_SETTLE: Duration = Duration::from_millis(30);

/// Grace period before the device is destroyed, so queued events are read.
const DRAIN_BEFORE_DESTROY: Duration = Duration::from_millis(100);

mod ioctl {
    nix::ioctl_none!(dev_create, b'U', 1);
    nix::ioctl_none!(dev_destroy, b'U', 2);
    nix::ioctl_write_int!(set_evbit, b'U', 100);
    nix::ioctl_write_int!(set_keybit, b'U', 101);
    nix::ioctl_write_int!(set_relbit, b'U', 102);
    nix::ioctl_write_int!(set_absbit, b'U', 103);
    nix::ioctl_write_int!(set_propbit, b'U', 110);
}

/// An absolute pointer on `/dev/uinput`. The compositor maps its full range
/// across the whole output layout, so on a multi-monitor setup coordinates
/// relative to a single output won't line up. Events are batched until
/// [`frame`](Self::frame).
pub struct UinputPointer {
    file: File,
    pending: RefCell<Vec<Event>>,
    warned: Cell<bool>,
}

impl UinputPointer {
    /// Create the device.
    pub fn new() -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .context("open /dev/uinput (needs write access)")?;
        let fd = file.as_raw_fd();

        // SAFETY: plain uinput ioctls on a descriptor we own, with by-value
        // int arguments.
        unsafe {
            ioctl::set_evbit(fd, EV_KEY as _)?;
            for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
                ioctl::set_keybit(fd, button as _)?;
            }
            ioctl::set_evbit(fd, EV_REL as _)?;
            for axis in [REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES] {
                ioctl::set_relbit(fd, axis as _)?;
            }
            ioctl::set_evbit(fd, EV_ABS as _)?;
            ioctl::set_absbit(fd, ABS_X as _)?;
            ioctl::set_absbit(fd, ABS_Y as _)?;
            ioctl::set_propbit(fd, INPUT_PROP_POINTER as _)?;
        }

        (&file)
            .write_all(&user_dev_setup())
            .context("configure uinput device")?;
        // SAFETY: as above.
        unsafe { ioctl::dev_create(fd)? };

        Ok(Self {
            file,
            pending: RefCell::new(Vec::new()),
            warned: Cell::new(false),
        })
    }

    /// `x_extent` x `y_extent` is the coordinate space `x`/`y` are in, as
    /// passed by the caller each time (it can change when the overlay moves
    /// to a differently sized output).
    fn motion_absolute(&self, x: u32, y: u32, x_extent: u32, y_extent: u32) {
        let mut pending = self.pending.borrow_mut();
        pending.push((EV_ABS, ABS_X, scale_abs(x, x_extent)));
        pending.push((EV_ABS, ABS_Y, scale_abs(y, y_extent)));
    }

    fn button(&self, button: u32, pressed: bool) {
        if let Some(event) = button_event(button, pressed) {
            self.pending.borrow_mut().push(event);
        }
    }

    fn scroll(&self, axis: Axis, discrete: i32) {
        self.pending
            .borrow_mut()
            .extend(scroll_events(axis, discrete));
    }

    fn frame(&self) {
        let events = std::mem::take(&mut *self.pending.borrow_mut());
        if events.is_empty() {
            return;
        }
        if let Err(e) = (&self.file).write_all(&encode_events(&events)) {
            // Warn once; a broken device would otherwise spam every event.
            if !self.warned.replace(true) {
                eprintln!("uinput pointer write failed: {e}");
            }
        }
        std::thread::sleep(settle_after(&events));
    }
}

/// A queued evdev event: (type, code, value).
type Event = (u16, u16, i32);

/// How long to wait after writing `events`: a pointer move needs the long
/// settle, a button edge the short one, anything else (wheel) none.
fn settle_after(events: &[Event]) -> Duration {
    if events.iter().any(|(ty, _, _)| *ty == EV_ABS) {
        MOTION_SETTLE
    } else if events.iter().any(|(ty, _, _)| *ty == EV_KEY) {
        EDGE_SETTLE
    } else {
        Duration::ZERO
    }
}

/// Map pixel `v` of a `0..extent` axis onto the device's `0..=ABS_RANGE`,
/// aiming at the pixel's centre. libinput maps a device value `d` to
/// `d * extent / (ABS_RANGE + 1)`, so spreading over `extent` (not
/// `extent - 1`) steps and adding half a pixel lands inside pixel `v` instead
/// of drifting up to a pixel right of it.
fn scale_abs(v: u32, extent: u32) -> i32 {
    let extent = f64::from(extent.max(1));
    let centre = f64::from(v).min(extent - 1.0) + 0.5;
    ((centre * f64::from(ABS_RANGE + 1) / extent).floor() as i32).min(ABS_RANGE)
}

/// The key event for a mouse button, or `None` for a button the device lacks.
fn button_event(button: u32, pressed: bool) -> Option<Event> {
    let code = [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE]
        .into_iter()
        .find(|code| u32::from(*code) == button)?;
    Some((EV_KEY, code, i32::from(pressed)))
}

/// Wheel events for `discrete` notches, following the wlr convention
/// (positive = down / right); evdev's vertical wheel is positive = up.
fn scroll_events(axis: Axis, discrete: i32) -> [Event; 2] {
    match axis {
        Axis::VerticalScroll => [
            (EV_REL, REL_WHEEL, -discrete),
            (EV_REL, REL_WHEEL_HI_RES, -discrete * 120),
        ],
        _ => [
            (EV_REL, REL_HWHEEL, discrete),
            (EV_REL, REL_HWHEEL_HI_RES, discrete * 120),
        ],
    }
}

/// Serialize `events` as `struct input_event`s followed by a `SYN_REPORT`.
fn encode_events(events: &[Event]) -> Vec<u8> {
    // The leading `struct timeval` is 16 bytes on 64-bit targets, 8 on 32-bit.
    let timeval = vec![0u8; std::mem::size_of::<nix::libc::timeval>()];
    let mut buf = Vec::with_capacity((events.len() + 1) * (timeval.len() + 8));
    for &(ty, code, value) in events.iter().chain(&[(EV_SYN, SYN_REPORT, 0)]) {
        // timeval (zeroed; the kernel stamps it), type, code, value.
        buf.extend_from_slice(&timeval);
        buf.extend_from_slice(&ty.to_ne_bytes());
        buf.extend_from_slice(&code.to_ne_bytes());
        buf.extend_from_slice(&value.to_ne_bytes());
    }
    buf
}

/// The legacy `struct uinput_user_dev`: name[80], input_id (4 x u16),
/// ff_effects_max, then absmax/absmin/absfuzz/absflat[ABS_CNT].
fn user_dev_setup() -> Vec<u8> {
    const ID: usize = 80;
    const ABSMAX: usize = ID + 8 + 4;
    let mut setup = vec![0u8; ABSMAX + 4 * ABS_CNT * 4];
    let name = b"stochos virtual pointer";
    setup[..name.len()].copy_from_slice(name);
    setup[ID..ID + 2].copy_from_slice(&0x06u16.to_ne_bytes()); // BUS_VIRTUAL
    setup[ID + 2..ID + 4].copy_from_slice(&0x1u16.to_ne_bytes()); // vendor
    setup[ID + 4..ID + 6].copy_from_slice(&0x1u16.to_ne_bytes()); // product
    setup[ID + 6..ID + 8].copy_from_slice(&0x1u16.to_ne_bytes()); // version
    for axis in [ABS_X, ABS_Y] {
        let at = ABSMAX + axis as usize * 4;
        setup[at..at + 4].copy_from_slice(&ABS_RANGE.to_ne_bytes());
    }
    setup
}

impl Drop for UinputPointer {
    fn drop(&mut self) {
        // Destroying the device discards events the compositor has not read
        // yet, and stochos exits right after its last click; give it a moment.
        std::thread::sleep(DRAIN_BEFORE_DESTROY);
        // SAFETY: destroying the device we created on our own descriptor.
        let _ = unsafe { ioctl::dev_destroy(self.file.as_raw_fd()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pixel libinput's mapping (`d * extent / 65536`) puts device value
    /// `d` in.
    fn pixel_for(d: i32, extent: u32) -> u32 {
        (d as u64 * u64::from(extent) / 65536) as u32
    }

    #[test]
    fn scale_abs_lands_inside_the_requested_pixel() {
        // The compositor's mapping of the scaled value must give back the
        // pixel asked for, at every pixel of several common widths. A mapping
        // over `extent - 1` steps drifts up to a pixel right of the target.
        for extent in [2, 640, 1366, 1920, 2560, 3440, 3840] {
            for v in 0..extent {
                assert_eq!(
                    pixel_for(scale_abs(v, extent), extent),
                    v,
                    "x={v} of {extent}"
                );
            }
        }
    }

    #[test]
    fn scale_abs_stays_within_the_device_range() {
        for extent in [1u32, 2, 1920, 3440] {
            for v in [
                0,
                extent / 2,
                extent.saturating_sub(1),
                extent,
                extent + 100,
                u32::MAX,
            ] {
                let scaled = scale_abs(v, extent);
                assert!(
                    (0..=ABS_RANGE).contains(&scaled),
                    "x={v} of {extent} -> {scaled}"
                );
            }
        }
    }

    #[test]
    fn scale_abs_is_monotonic() {
        let scaled: Vec<i32> = (0..3440).map(|v| scale_abs(v, 3440)).collect();
        assert!(scaled.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn settle_depends_on_the_kind_of_event() {
        assert_eq!(settle_after(&[(EV_ABS, ABS_X, 1)]), MOTION_SETTLE);
        // A batch that moves is a move even if it also has a button.
        assert_eq!(
            settle_after(&[(EV_KEY, BTN_LEFT, 1), (EV_ABS, ABS_X, 1)]),
            MOTION_SETTLE
        );
        assert_eq!(settle_after(&[(EV_KEY, BTN_LEFT, 1)]), EDGE_SETTLE);
        assert_eq!(settle_after(&[(EV_REL, REL_WHEEL, 1)]), Duration::ZERO);
        assert!(EDGE_SETTLE < MOTION_SETTLE);
    }

    #[test]
    fn a_double_click_stays_inside_the_double_click_window() {
        // Press, release, press, release at EDGE_SETTLE apart: the second
        // release must land well inside a typical 400 ms double-click time.
        assert!(EDGE_SETTLE * 3 < Duration::from_millis(400));
    }

    /// A pointer whose events go to /dev/null, to exercise the queueing
    /// without a uinput device.
    fn null_pointer() -> UinputPointer {
        UinputPointer {
            file: OpenOptions::new().write(true).open("/dev/null").unwrap(),
            pending: RefCell::new(Vec::new()),
            warned: Cell::new(false),
        }
    }

    #[test]
    fn motion_scales_against_the_extents_passed_each_time() {
        let vp = null_pointer();
        vp.motion_absolute(100, 50, 200, 100);
        vp.motion_absolute(100, 50, 400, 200);
        let pending = vp.pending.borrow().clone();
        assert_eq!(pending[0], (EV_ABS, ABS_X, scale_abs(100, 200)));
        assert_eq!(pending[1], (EV_ABS, ABS_Y, scale_abs(50, 100)));
        // The same pixel in a larger space lands nearer the origin.
        assert_eq!(pending[2], (EV_ABS, ABS_X, scale_abs(100, 400)));
        assert!(pending[2].2 < pending[0].2);
    }

    #[test]
    fn frame_drains_the_queue() {
        let vp = null_pointer();
        vp.motion_absolute(1, 1, 10, 10);
        vp.button(0x110, true);
        vp.button(0x113, true); // unsupported, never queued
        assert_eq!(vp.pending.borrow().len(), 3);
        vp.frame();
        assert!(vp.pending.borrow().is_empty());
    }

    #[test]
    fn scale_abs_clamps_out_of_range_coordinates_to_the_last_pixel() {
        assert_eq!(scale_abs(5000, 1920), scale_abs(1919, 1920));
    }

    #[test]
    fn button_event_accepts_the_three_buttons() {
        assert_eq!(button_event(0x110, true), Some((EV_KEY, BTN_LEFT, 1)));
        assert_eq!(button_event(0x111, false), Some((EV_KEY, BTN_RIGHT, 0)));
        assert_eq!(button_event(0x112, true), Some((EV_KEY, BTN_MIDDLE, 1)));
    }

    #[test]
    fn button_event_ignores_other_buttons() {
        assert_eq!(button_event(0x113, true), None); // BTN_SIDE
        assert_eq!(button_event(0, true), None);
        // Must not be fooled by the low 16 bits of a larger value.
        assert_eq!(button_event(0x1_0110, true), None);
    }

    #[test]
    fn vertical_scroll_inverts_the_wlr_direction() {
        // wlr: positive discrete = down; evdev REL_WHEEL: positive = up.
        let down = scroll_events(Axis::VerticalScroll, 1);
        assert_eq!(down[0], (EV_REL, REL_WHEEL, -1));
        assert_eq!(down[1], (EV_REL, REL_WHEEL_HI_RES, -120));
        let up = scroll_events(Axis::VerticalScroll, -1);
        assert_eq!(up[0], (EV_REL, REL_WHEEL, 1));
        assert_eq!(up[1], (EV_REL, REL_WHEEL_HI_RES, 120));
    }

    #[test]
    fn horizontal_scroll_keeps_the_wlr_direction() {
        let right = scroll_events(Axis::HorizontalScroll, 1);
        assert_eq!(right[0], (EV_REL, REL_HWHEEL, 1));
        assert_eq!(right[1], (EV_REL, REL_HWHEEL_HI_RES, 120));
        let left = scroll_events(Axis::HorizontalScroll, -2);
        assert_eq!(left[0], (EV_REL, REL_HWHEEL, -2));
        assert_eq!(left[1], (EV_REL, REL_HWHEEL_HI_RES, -240));
    }

    #[test]
    fn encode_events_appends_a_syn_report() {
        let buf = encode_events(&[(EV_KEY, BTN_LEFT, 1)]);
        let timeval = std::mem::size_of::<nix::libc::timeval>();
        let event = timeval + 8;
        assert_eq!(buf.len(), 2 * event);
        let field = |n: usize| {
            let at = n * event + timeval;
            (
                u16::from_ne_bytes([buf[at], buf[at + 1]]),
                u16::from_ne_bytes([buf[at + 2], buf[at + 3]]),
                i32::from_ne_bytes([buf[at + 4], buf[at + 5], buf[at + 6], buf[at + 7]]),
            )
        };
        assert_eq!(field(0), (EV_KEY, BTN_LEFT, 1));
        assert_eq!(field(1), (EV_SYN, SYN_REPORT, 0));
        // The timeval is left zeroed for the kernel to stamp.
        assert!(buf[..timeval].iter().all(|b| *b == 0));
    }

    #[test]
    fn user_dev_setup_has_the_legacy_layout() {
        let setup = user_dev_setup();
        assert_eq!(setup.len(), 1116); // sizeof(struct uinput_user_dev)
        assert!(setup.starts_with(b"stochos virtual pointer\0"));
        let absmax = |axis: usize| {
            let at = 92 + axis * 4;
            i32::from_ne_bytes([setup[at], setup[at + 1], setup[at + 2], setup[at + 3]])
        };
        assert_eq!(absmax(ABS_X as usize), ABS_RANGE);
        assert_eq!(absmax(ABS_Y as usize), ABS_RANGE);
        assert_eq!(absmax(2), 0);
    }
}
