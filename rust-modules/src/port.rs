//! The webOS port: the one table that fills the `tv` interfaces, and the C entry that installs it.
//!
//! Everything outside this module reaches the television through `nj_platform::tv`; this is the only
//! place that names `webos`, `keymanager`, `system` and `player::ffi` together. Its functions keep
//! their own `cfg(any(hostsim, test))` arms, so the simulator installs this same table and behaves
//! as it always did.
use std::os::raw::{c_char, c_int};

#[cfg(all(not(feature = "hostsim"), not(test)))]
const SINK: &dyn nj_platform::tv::sink::VideoSink = &crate::player::ffi::StarfishSink;
#[cfg(feature = "hostsim")]
const SINK: &dyn nj_platform::tv::sink::VideoSink = &crate::player::ffi_host::HostSink;
#[cfg(all(not(feature = "hostsim"), test))]
const SINK: &dyn nj_platform::tv::sink::VideoSink = &nj_platform::tv::sink::NoSink;

static PORT: nj_platform::tv::Port = nj_platform::tv::Port {
    probe_device: nj_platform::webos::probe,
    start_capability_probe: nj_platform::webos::caps::start_probe,
    repair_sandbox: nj_platform::webos::jail_repair::execute,
    seal: nj_platform::keymanager::seal,
    open: nj_platform::keymanager::open,
    remove: nj_platform::keymanager::remove,
    system_locale: nj_platform::webos::system_locale,
    go_home: nj_platform::webos::go_home,
    poll_home: nj_platform::webos::poll_home,
    launch_app: nj_platform::webos::launch_app,
    deliver_toast: nj_platform::webos::toast::deliver,
    bind_window: nj_platform::webos::bind_window,
    grab_surface: crate::system::sys_grab_wayland,
    release_surface: crate::system::sys_release_wayland,
    arm_opaque_region: crate::system::opaque_region_init,
    opaque_route: crate::system::opaque_route,
    clear_opaque_region: crate::system::clear_opaque_region,
    pump_bus: crate::system::ls2_pump,
    frame_probe_request: crate::system::frame_probe_request,
    frame_probe_waiting: crate::system::frame_probe_waiting,
    frame_probe_acquired: crate::system::frame_probe_acquired,
    frame_probe_fields: crate::system::frame_probe_fields,
    refresh_volume: nj_platform::webos::volume::refresh,
    sink: SINK,
};

/// The C shim's entry (`src/main.c`) and the simulator's (`src/bin/sim.rs`): install the port, then
/// hand over to `app::run_application`.
#[no_mangle]
pub extern "C" fn nj_run(pms_host: *const c_char, pms_port: c_int) -> c_int {
    let _ = nj_platform::tv::install(&PORT);
    #[cfg(target_os = "linux")]
    let _ = nj_platform::storage::client::install_activator(nj_platform::webos::activate_storage_helper);
    crate::app::run_application(pms_host, pms_port)
}
