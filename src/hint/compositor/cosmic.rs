//! COSMIC provider: window geometry from `zcosmic_toplevel_info_v1`.
//!
//! Toplevels are enumerated through `ext_foreign_toplevel_list_v1` and each is
//! upgraded to a cosmic handle, which reports its geometry relative to an
//! output. That is shifted by the output's `wl_output` position to give
//! absolute screen coordinates. The protocol does not report a pid, so
//! windows are matched to AT-SPI applications by size and title.

use std::time::{Duration, Instant};

use wayland_client::{
    event_created_child,
    protocol::{wl_output, wl_registry},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1, ext_foreign_toplevel_list_v1,
};

use super::{CompositorSnapshot, WindowRect};

#[allow(
    dead_code,
    non_camel_case_types,
    non_upper_case_globals,
    unused_imports
)]
#[allow(missing_docs, clippy::all)]
mod protocol {
    use wayland_client;
    use wayland_client::protocol::*;
    use wayland_protocols::ext::foreign_toplevel_list::v1::client::*;

    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols::ext::foreign_toplevel_list::v1::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/cosmic-toplevel-info-unstable-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/cosmic-toplevel-info-unstable-v1.xml");
}

use protocol::{zcosmic_toplevel_handle_v1 as handle, zcosmic_toplevel_info_v1 as info};

/// `zcosmic_toplevel_handle_v1::state` value for an activated (focused) window.
const STATE_ACTIVATED: u32 = 2;
/// `zcosmic_toplevel_handle_v1::state` value for a minimized window.
const STATE_MINIMIZED: u32 = 1;

/// How long to wait for every toplevel's initial cosmic events.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(500);
/// Extra time for geometry once every toplevel's `state` has arrived.
const GEOMETRY_GRACE: Duration = Duration::from_millis(50);
const SETTLE_POLL: Duration = Duration::from_millis(10);

/// A window rectangle: x, y, width, height.
type Rect = (i32, i32, i32, i32);

#[derive(Default)]
struct Toplevel {
    title: String,
    /// The cosmic handle's first `state` event has arrived.
    initialized: bool,
    activated: bool,
    minimized: bool,
    /// Latest geometry per output (keyed by the output's protocol id).
    geometry: Vec<(u32, Rect)>,
}

#[derive(Default)]
struct State {
    info: Option<info::ZcosmicToplevelInfoV1>,
    list: Option<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1>,
    /// Global positions of bound outputs, keyed by protocol id.
    outputs: Vec<(u32, (i32, i32))>,
    /// Foreign handle id -> its toplevel data (title lives on the foreign handle).
    toplevels: Vec<(u32, Toplevel)>,
}

impl Toplevel {
    /// Everything needed to place the window has arrived: its state and, unless
    /// it is minimized, a geometry.
    fn ready(&self) -> bool {
        self.initialized && (self.minimized || !self.geometry.is_empty())
    }
}

impl State {
    /// An already-known toplevel. Events for one that has since closed are
    /// dropped rather than resurrecting it.
    fn toplevel_mut(&mut self, id: u32) -> Option<&mut Toplevel> {
        self.toplevels
            .iter_mut()
            .find(|(k, _)| *k == id)
            .map(|(_, t)| t)
    }
}

pub fn snapshot() -> Option<CompositorSnapshot> {
    let conn = Connection::connect_to_env().ok()?;
    let mut eq: EventQueue<State> = conn.new_event_queue();
    let qh = eq.handle();
    let mut state = State::default();
    conn.display().get_registry(&qh, ());
    // Globals, then outputs and foreign toplevels (which request their cosmic
    // handles).
    for _ in 0..3 {
        eq.roundtrip(&mut state).ok()?;
    }
    state.info.as_ref()?;
    // The compositor sends a cosmic handle's initial events asynchronously,
    // not within the roundtrip that requested it, and `zcosmic_toplevel_info`
    // sends no `done` for them. `state` arrives first, then `geometry`; wait
    // for both, but once every `state` is in, only give geometry a short grace
    // so a window that never gets one can't stall every hint launch.
    let started = Instant::now();
    let mut initialized_at = None;
    loop {
        if state.toplevels.iter().all(|(_, t)| t.ready()) {
            break;
        }
        let now = Instant::now();
        if state.toplevels.iter().all(|(_, t)| t.initialized) {
            let since = *initialized_at.get_or_insert(now);
            if now.duration_since(since) >= GEOMETRY_GRACE {
                break;
            }
        }
        if now.duration_since(started) >= SETTLE_TIMEOUT {
            break;
        }
        std::thread::sleep(SETTLE_POLL);
        eq.roundtrip(&mut state).ok()?;
    }

    Some(build_snapshot(&state.toplevels, &state.outputs))
}

/// Turn collected toplevels into absolute-coordinate window rectangles,
/// skipping minimized windows and ones with no usable geometry.
fn build_snapshot(
    toplevels: &[(u32, Toplevel)],
    outputs: &[(u32, (i32, i32))],
) -> CompositorSnapshot {
    let mut snapshot = CompositorSnapshot::default();
    for (_, t) in toplevels {
        if t.minimized {
            continue;
        }
        // A window can span outputs; use its largest visible geometry.
        let rect = t
            .geometry
            .iter()
            .filter_map(|(out, (x, y, w, h))| {
                let (ox, oy) = outputs
                    .iter()
                    .find(|(id, _)| id == out)
                    .map_or((0, 0), |(_, p)| *p);
                (*w > 0 && *h > 0).then_some((x.saturating_add(ox), y.saturating_add(oy), *w, *h))
            })
            .max_by_key(|(_, _, w, h)| i64::from(*w) * i64::from(*h));
        let Some((x, y, w, h)) = rect else { continue };
        if t.activated {
            snapshot.active = Some(snapshot.windows.len());
        }
        snapshot.windows.push(WindowRect {
            x,
            y,
            w,
            h,
            title: t.title.clone(),
            pid: None,
        });
    }
    snapshot
}

/// Decode the `state` event's array of native-endian `u32` state values.
fn decode_states(raw: &[u8]) -> Vec<u32> {
    raw.as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_ne_bytes(*c))
        .collect()
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "zcosmic_toplevel_info_v1" if version >= 2 => {
                state.info = Some(registry.bind(name, version.min(3), qh, ()));
            }
            "ext_foreign_toplevel_list_v1" => {
                state.list = Some(registry.bind(name, 1, qh, ()));
            }
            "wl_output" => {
                let _: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Geometry { x, y, .. } = event {
            let id = output.id().protocol_id();
            state.outputs.retain(|(k, _)| *k != id);
            state.outputs.push((id, (x, y)));
        }
    }
}

impl Dispatch<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            let id = toplevel.id().protocol_id();
            state.toplevels.push((id, Toplevel::default()));
            if let Some(info) = &state.info {
                info.get_cosmic_toplevel(&toplevel, qh, id);
            }
        }
    }

    event_created_child!(State, ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = handle.id().protocol_id();
        match event {
            ext_foreign_toplevel_handle_v1::Event::Title { title } => {
                if let Some(t) = state.toplevel_mut(id) {
                    t.title = title;
                }
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                state.toplevels.retain(|(k, _)| *k != id);
            }
            _ => {}
        }
    }
}

impl Dispatch<info::ZcosmicToplevelInfoV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &info::ZcosmicToplevelInfoV1,
        _: info::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// User data is the owning foreign handle's protocol id.
impl Dispatch<handle::ZcosmicToplevelHandleV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &handle::ZcosmicToplevelHandleV1,
        event: handle::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            handle::Event::State { state: raw } => {
                let states = decode_states(&raw);
                if let Some(t) = state.toplevel_mut(*id) {
                    t.initialized = true;
                    t.activated = states.contains(&STATE_ACTIVATED);
                    t.minimized = states.contains(&STATE_MINIMIZED);
                }
            }
            handle::Event::Geometry {
                output,
                x,
                y,
                width,
                height,
            } => {
                let out = output.id().protocol_id();
                if let Some(t) = state.toplevel_mut(*id) {
                    t.geometry.retain(|(k, _)| *k != out);
                    t.geometry.push((out, (x, y, width, height)));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toplevel(title: &str, geometry: &[(u32, Rect)]) -> Toplevel {
        Toplevel {
            title: title.to_owned(),
            initialized: true,
            geometry: geometry.to_vec(),
            ..Toplevel::default()
        }
    }

    #[test]
    fn single_output_uses_geometry_as_is() {
        let toplevels = [(1, toplevel("a", &[(5, (257, 111, 1720, 1096))]))];
        let snap = build_snapshot(&toplevels, &[(5, (0, 0))]);
        assert_eq!(snap.windows.len(), 1);
        let w = &snap.windows[0];
        assert_eq!((w.x, w.y, w.w, w.h), (257, 111, 1720, 1096));
        assert_eq!(w.title, "a");
        assert_eq!(w.pid, None);
        assert!(snap.active.is_none());
    }

    #[test]
    fn output_position_is_added_to_geometry() {
        let toplevels = [(1, toplevel("a", &[(7, (10, 20, 300, 200))]))];
        let snap = build_snapshot(&toplevels, &[(5, (0, 0)), (7, (3440, -100))]);
        let w = &snap.windows[0];
        assert_eq!((w.x, w.y), (3450, -80));
    }

    #[test]
    fn unknown_output_is_treated_as_the_origin() {
        let toplevels = [(1, toplevel("a", &[(9, (10, 20, 300, 200))]))];
        let snap = build_snapshot(&toplevels, &[(5, (1000, 1000))]);
        let w = &snap.windows[0];
        assert_eq!((w.x, w.y), (10, 20));
    }

    #[test]
    fn minimized_windows_are_skipped() {
        let mut hidden = toplevel("hidden", &[(5, (0, 0, 100, 100))]);
        hidden.minimized = true;
        let toplevels = [
            (1, hidden),
            (2, toplevel("shown", &[(5, (0, 0, 100, 100))])),
        ];
        let snap = build_snapshot(&toplevels, &[(5, (0, 0))]);
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(snap.windows[0].title, "shown");
    }

    #[test]
    fn windows_without_usable_geometry_are_skipped() {
        let toplevels = [
            (1, toplevel("none", &[])),
            (2, toplevel("empty", &[(5, (0, 0, 0, 50))])),
            (3, toplevel("ok", &[(5, (0, 0, 50, 50))])),
        ];
        let snap = build_snapshot(&toplevels, &[(5, (0, 0))]);
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(snap.windows[0].title, "ok");
    }

    #[test]
    fn active_index_follows_skipped_windows() {
        let mut focused = toplevel("focused", &[(5, (0, 0, 100, 100))]);
        focused.activated = true;
        let toplevels = [
            (1, toplevel("none", &[])),
            (2, toplevel("other", &[(5, (0, 0, 100, 100))])),
            (3, focused),
        ];
        let snap = build_snapshot(&toplevels, &[(5, (0, 0))]);
        assert_eq!(snap.windows.len(), 2);
        assert_eq!(snap.active_window().unwrap().title, "focused");
    }

    #[test]
    fn a_window_on_two_outputs_uses_its_larger_part() {
        let toplevels = [(
            1,
            toplevel(
                "spanning",
                &[(5, (3000, 0, 440, 800)), (7, (0, 0, 1000, 800))],
            ),
        )];
        let snap = build_snapshot(&toplevels, &[(5, (0, 0)), (7, (3440, 0))]);
        let w = &snap.windows[0];
        assert_eq!((w.x, w.w), (3440, 1000));
    }

    #[test]
    fn decode_states_reads_native_endian_u32s() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&STATE_ACTIVATED.to_ne_bytes());
        raw.extend_from_slice(&STATE_MINIMIZED.to_ne_bytes());
        assert_eq!(decode_states(&raw), [STATE_ACTIVATED, STATE_MINIMIZED]);
        assert!(decode_states(&[]).is_empty());
    }

    #[test]
    fn ready_needs_state_and_geometry() {
        let mut t = Toplevel::default();
        assert!(!t.ready());
        t.initialized = true;
        assert!(!t.ready(), "state alone is not enough for a visible window");
        t.geometry.push((5, (0, 0, 10, 10)));
        assert!(t.ready());
    }

    #[test]
    fn minimized_window_is_ready_without_geometry() {
        let t = Toplevel {
            initialized: true,
            minimized: true,
            ..Toplevel::default()
        };
        assert!(t.ready());
    }

    #[test]
    fn events_for_a_closed_toplevel_are_dropped() {
        let mut state = State::default();
        state.toplevels.push((1, Toplevel::default()));
        assert!(state.toplevel_mut(1).is_some());
        state.toplevels.clear();
        assert!(state.toplevel_mut(1).is_none());
        assert!(state.toplevels.is_empty());
    }

    #[test]
    fn extreme_coordinates_do_not_overflow() {
        let toplevels = [(1, toplevel("a", &[(5, (i32::MAX, 0, 10, 10))]))];
        let snap = build_snapshot(&toplevels, &[(5, (100, 0))]);
        assert_eq!(snap.windows[0].x, i32::MAX);
    }

    #[test]
    fn decode_states_ignores_a_trailing_partial_value() {
        let mut raw = STATE_ACTIVATED.to_ne_bytes().to_vec();
        raw.extend_from_slice(&[1, 2]);
        assert_eq!(decode_states(&raw), [STATE_ACTIVATED]);
    }
}
