use std::{
  collections::HashMap,
  sync::{LazyLock, Mutex},
};

use log::debug;
use wayland_client::{
  Connection as WaylandConnection,
  Dispatch,
  Proxy,
  QueueHandle,
  backend::ObjectId,
  protocol::wl_registry,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
  zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
  zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

static FOCUSED_APP: Mutex<Option<String>> = Mutex::new(None);
static TOPLEVEL_APPS: LazyLock<Mutex<HashMap<ObjectId, String>>> =
  LazyLock::new(|| Mutex::new(HashMap::new()));

/// Initialize Wayland state for window management in a background thread
pub fn init_wayland_state() {
  std::thread::spawn(|| {
    if let Err(e) = run_wayland_event_loop() {
      debug!("Wayland event loop error: {e}");
    }
  });
}

/// Get the currently focused window application name using Wayland protocols
pub fn get_focused_window_app() -> Option<String> {
  // Try Wayland protocol first
  if let Ok(focused) = FOCUSED_APP.lock()
    && let Some(ref app) = *focused {
      debug!("Found focused app via Wayland protocol: {app}");
      return Some(app.clone());
    }

  debug!("No focused window detection method worked");
  None
}

/// Run the Wayland event loop
fn run_wayland_event_loop() -> Result<(), Box<dyn std::error::Error>> {
  let conn = match WaylandConnection::connect_to_env() {
    Ok(conn) => conn,
    Err(e) => {
      debug!("Failed to connect to Wayland: {e}");
      return Ok(());
    },
  };

  let display = conn.display();
  let mut event_queue = conn.new_event_queue();
  let qh = event_queue.handle();

  let _registry = display.get_registry(&qh, ());

  loop {
    event_queue.blocking_dispatch(&mut AppState)?;
  }
}

struct AppState;

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
  fn event(
    _state: &mut Self,
    registry: &wl_registry::WlRegistry,
    event: wl_registry::Event,
    _data: &(),
    _conn: &WaylandConnection,
    qh: &QueueHandle<Self>,
  ) {
    if let wl_registry::Event::Global {
      name,
      interface,
      version: _,
    } = event
      && interface == "zwlr_foreign_toplevel_manager_v1" {
        let _manager: ZwlrForeignToplevelManagerV1 =
          registry.bind(name, 1, qh, ());
      }
  }

  fn event_created_child(
    _opcode: u16,
    qhandle: &QueueHandle<Self>,
  ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
    qhandle.make_data::<ZwlrForeignToplevelManagerV1, ()>(())
  }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for AppState {
  fn event(
    _state: &mut Self,
    _manager: &ZwlrForeignToplevelManagerV1,
    event: zwlr_foreign_toplevel_manager_v1::Event,
    _data: &(),
    _conn: &WaylandConnection,
    _qh: &QueueHandle<Self>,
  ) {
    if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } =
      event
    {
      // New toplevel created
      // We'll track it for focus events
      let _: ZwlrForeignToplevelHandleV1 = toplevel;
    }
  }

  fn event_created_child(
    _opcode: u16,
    qhandle: &QueueHandle<Self>,
  ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
    qhandle.make_data::<ZwlrForeignToplevelHandleV1, ()>(())
  }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for AppState {
  fn event(
    _state: &mut Self,
    handle: &ZwlrForeignToplevelHandleV1,
    event: zwlr_foreign_toplevel_handle_v1::Event,
    _data: &(),
    _conn: &WaylandConnection,
    _qh: &QueueHandle<Self>,
  ) {
    let handle_id = handle.id();

    match event {
      zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
        debug!("Toplevel app_id: {app_id}");
        // Store the app_id for this handle
        if let Ok(mut apps) = TOPLEVEL_APPS.lock() {
          apps.insert(handle_id, app_id);
        }
      },
      zwlr_foreign_toplevel_handle_v1::Event::State {
        state: toplevel_state,
      } => {
        // Check if this toplevel is activated (focused)
        let states: Vec<u8> = toplevel_state;
        // Check for activated state (value 2 in the enum)
        if states.chunks_exact(4).any(|chunk| {
          u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) == 2
        }) {
          debug!("Toplevel activated");
          // Update focused app to the `app_id` of this handle
          if let (Ok(apps), Ok(mut focused)) =
            (TOPLEVEL_APPS.lock(), FOCUSED_APP.lock())
            && let Some(app_id) = apps.get(&handle_id) {
              debug!("Setting focused app to: {app_id}");
              *focused = Some(app_id.clone());
            }
        }
      },
      zwlr_foreign_toplevel_handle_v1::Event::Closed => {
        // Clean up when toplevel is closed
        if let Ok(mut apps) = TOPLEVEL_APPS.lock() {
          apps.remove(&handle_id);
        }
      },
      _ => {},
    }
  }

  fn event_created_child(
    _opcode: u16,
    qhandle: &QueueHandle<Self>,
  ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
    qhandle.make_data::<ZwlrForeignToplevelHandleV1, ()>(())
  }
}
