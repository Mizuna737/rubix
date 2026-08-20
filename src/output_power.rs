//! wlr-output-power-management-unstable-v1 (`zwlr_output_power_manager_v1` /
//! `zwlr_output_power_v1`).
//!
//! Smithay 0.7 ships no helper for this protocol (only `idle_inhibit` and
//! `idle_notify` live under `smithay::wayland`) -- same situation as
//! wlr-screencopy, so this hand-rolls `GlobalDispatch`/`Dispatch` on the raw
//! bindings smithay re-exports from `wayland-protocols-wlr`. See
//! `screencopy.rs`'s doc comment for the general shape; this module mirrors
//! its structure.
//!
//! ## Single choke point for `mode`
//!
//! Every power transition in Rubix -- the Phase 2 idle timer, wake-on-input,
//! the `rubix screen` CLI (via IPC), and this protocol's own `set_mode` --
//! funnels through `RubixState::set_screen_power` (state.rs), which calls
//! back into [`notify_power_changed`] here after any change that actually
//! did something. That is deliberate: the classic bug in these
//! implementations is an internal blank (an idle timeout, say) that changes
//! the hardware but never tells a bound `zwlr_output_power_v1` client, so
//! `wlopm`/waybar reports a lit screen while the panel is dark. Routing every
//! source through one function that ends in `notify_power_changed` is what
//! rules that out structurally rather than by remembering to call it at each
//! call site.
//!
//! ## One live control object per output
//!
//! `RubixState::output_power: HashMap<String, ZwlrOutputPowerV1>` (keyed by
//! output name, the same identity `udev::set_screen_power`'s
//! `output_name: Option<&str>` already uses) tracks the single object
//! currently allowed to drive each output. A second `get_output_power` for
//! an output that already has one gets an immediate `failed` and is never
//! registered, so it can never race the first for control. The registered
//! object is freed on `destroy` **and** on client disconnect/crash -- both
//! go through `Dispatch::destroyed`, which wayland-server calls on every
//! teardown path (the same reasoning as idle-inhibit's
//! `CompositorHandler::destroyed` in handlers/compositor.rs), not just the
//! explicit-request path -- so a crashed client can never permanently wedge
//! `get_output_power` for the output it was holding.

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::{
    zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
    zwlr_output_power_v1::{self, Mode, ZwlrOutputPowerV1},
};
use smithay::reexports::wayland_server::{
    backend::ClientId, protocol::wl_output::WlOutput, Client, DataInit, Dispatch, DisplayHandle,
    GlobalDispatch, New, Resource, WEnum,
};

use crate::RubixState;

/// Per-`zwlr_output_power_v1` Dispatch user-data. Just the target output's
/// name -- the request/destroyed handlers below look it up in
/// `RubixState::output_power` on every call rather than trusting a captured
/// "am I the live one" bool, so a superseded-and-failed object (see
/// `create_power`) reliably stays inert instead of racing whatever replaced
/// it.
pub(crate) struct OutputPowerData {
    output_name: String,
}

/// Advertise the manager global. Called from `RubixState::new` alongside the
/// other globals (state.rs).
pub(crate) fn init(dh: &DisplayHandle) {
    dh.create_global::<RubixState, ZwlrOutputPowerManagerV1, ()>(1, ());
}

impl GlobalDispatch<ZwlrOutputPowerManagerV1, ()> for RubixState {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputPowerManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrOutputPowerManagerV1, ()> for RubixState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _manager: &ZwlrOutputPowerManagerV1,
        request: zwlr_output_power_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_output_power_manager_v1::Request;
        match request {
            Request::GetOutputPower { id, output } => create_power(state, id, &output, data_init),
            // Destructor. Per the XML: "All objects created by the manager
            // will still remain valid, until their appropriate destroy
            // request has been called" -- nothing to do here, matches
            // screencopy.rs's manager-destroy handling.
            Request::Destroy => {}
            _ => {}
        }
    }
}

/// Resolve the target output, reject with `failed` if it's unknown or
/// already has a live control object, else register the new one and send
/// its current `mode` immediately (required: "This event is also sent
/// immediately when the object is created").
fn create_power(
    state: &mut RubixState,
    id: New<ZwlrOutputPowerV1>,
    wl_output: &WlOutput,
    data_init: &mut DataInit<'_, RubixState>,
) {
    // Every branch below has to answer the `new_id` with a live object
    // (the protocol has no way to refuse creation outright), so every path
    // ends in `data_init.init` -- only whether it's then immediately failed
    // differs.
    let Some(output) = Output::from_resource(wl_output) else {
        let power = data_init.init(id, OutputPowerData { output_name: String::new() });
        power.failed();
        return;
    };
    let name = output.name();

    if state.output_power.contains_key(&name) {
        // Another client (or an earlier object from this same client) is
        // already the live control for this output.
        let power = data_init.init(id, OutputPowerData { output_name: name });
        power.failed();
        return;
    }

    // Read the current state BEFORE creating the object: `output_power`'s
    // slot is filled last, and `output_is_off` borrows all of `state`
    // (not just that field), so it can't run while an in-progress
    // `entry()`-style borrow of `output_power` is still live.
    let off = state.output_is_off(&name);
    let power = data_init.init(id, OutputPowerData { output_name: name.clone() });
    power.mode(if off { Mode::Off } else { Mode::On });
    state.output_power.insert(name, power);
}

impl Dispatch<ZwlrOutputPowerV1, OutputPowerData> for RubixState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputPowerV1,
        request: zwlr_output_power_v1::Request,
        data: &OutputPowerData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_output_power_v1::Request;
        let Request::SetMode { mode } = request else {
            // `Destroy` (destructor) and anything unknown: nothing to do
            // here -- registry cleanup happens uniformly in `destroyed`
            // below, which fires for this too.
            return;
        };

        // Only the currently-registered object for this output may act -- a
        // superseded/failed object, or one whose output already disappeared
        // (`output_removed` already removed it from the registry), must stay
        // inert rather than reaching back into hardware it no longer owns.
        if state.output_power.get(&data.output_name) != Some(resource) {
            return;
        }

        let mode = match mode {
            WEnum::Value(m) => m,
            WEnum::Unknown(v) => {
                resource.post_error(
                    zwlr_output_power_v1::Error::InvalidMode,
                    format!("unknown power mode {v}"),
                );
                return;
            }
        };

        // `set_screen_power` is the single choke point (see module doc): it
        // performs the DRM transition AND calls `notify_power_changed` on
        // success, which will echo `mode` back to this very object -- exactly
        // what the protocol expects ("sent after an output changed its power
        // management mode... reason can be a client using set_mode").
        state.set_screen_power(matches!(mode, Mode::On), Some(&data.output_name));
    }

    fn destroyed(state: &mut Self, _client: ClientId, resource: &ZwlrOutputPowerV1, data: &OutputPowerData) {
        // Fires for explicit `destroy` AND for client disconnect/crash alike
        // -- see the module doc comment. Protocol: "Destroying the object
        // does NOT change output power state" -- this only frees the
        // per-output registry slot so a fresh `get_output_power` for the
        // same output isn't wrongly rejected.
        if state.output_power.get(&data.output_name) == Some(resource) {
            state.output_power.remove(&data.output_name);
        }
    }
}

/// Called from `RubixState::set_screen_power` after any change that actually
/// transitioned at least one output (an empty `changed` means the call was a
/// no-op and this is skipped entirely -- see `udev::set_screen_power`'s doc).
/// The single place that turns a power transition into a wire event,
/// regardless of what caused it.
pub(crate) fn notify_power_changed(state: &mut RubixState, changed: &[(Output, bool)]) {
    for (output, off) in changed {
        if let Some(power) = state.output_power.get(&output.name()) {
            power.mode(if *off { Mode::Off } else { Mode::On });
        }
    }
}

/// An output disconnected (hotplug/unplug): fail any live control object for
/// it -- "The output disappeared" is one of the XML's own listed `failed`
/// reasons -- and drop the registry slot so it doesn't wedge forever.
pub(crate) fn output_removed(state: &mut RubixState, output_name: &str) {
    if let Some(power) = state.output_power.remove(output_name) {
        power.failed();
    }
}
