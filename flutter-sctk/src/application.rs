use std::{collections::HashMap, fmt::Debug, rc::Rc, sync::Arc};

use calloop::futures::{Executor, Scheduler};
use flutter_engine::{
    builder::FlutterEngineBuilder, ffi::FlutterEngineDisplay, plugins::PluginRegistrar,
    CreateError, FlutterEngine,
};
use flutter_plugins::{
    isolate::IsolatePlugin, keyevent::KeyEventPlugin, lifecycle::LifecyclePlugin,
    localization::LocalizationPlugin, mousecursor::MouseCursorPlugin, navigation::NavigationPlugin,
    platform::PlatformPlugin, settings::SettingsPlugin, system::SystemPlugin,
    textinput::TextInputPlugin,
};
use flutter_runner_api::ApplicationAttributes;
use log::{error, trace, warn};
use parking_lot::{Mutex, RwLock};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, SurfaceData},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            self,
            timer::{TimeoutAction, Timer},
            EventLoop, LoopHandle, LoopSignal,
        },
        calloop_wayland_source::WaylandSource,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerHandler, ThemeSpec},
        Capability, SeatHandler, SeatState,
    },
    shell::xdg::{
        window::{Window, WindowConfigure, WindowHandler},
        XdgShell,
    },
    shm::{Shm, ShmHandler},
};
use thiserror::Error;
use wayland_backend::client::ObjectId;
use wayland_client::{
    globals::{registry_queue_init, BindError, GlobalError},
    protocol::{
        wl_keyboard::WlKeyboard,
        wl_output::{Transform, WlOutput},
        wl_pointer::WlPointer,
        wl_seat::WlSeat,
        wl_surface::WlSurface,
    },
    ConnectError, Connection, Proxy, QueueHandle,
};

use crate::{
    handler::{
        get_flutter_frame_time_nanos, SctkAsyncResult, SctkMouseCursorHandler, SctkPlatformHandler,
        SctkPlatformTaskHandler, SctkTextInputHandler, SctkVsyncHandler,
        FRAME_INTERVAL_60_HZ_IN_NANOS,
    },
    output::SctkOutput,
    window::{SctkFlutterWindow, SctkFlutterWindowCreateError},
};

pub struct SctkApplication {
    event_loop: EventLoop<'static, SctkApplicationState>,
    state: SctkApplicationState,
}

pub struct SctkApplicationState {
    conn: Connection,
    loop_handle: LoopHandle<'static, SctkApplicationState>,
    loop_signal: LoopSignal,
    registry_state: RegistryState,
    compositor_state: CompositorState,
    shm_state: Shm,
    output_state: OutputState,
    seat_state: SeatState,
    engine: FlutterEngine,
    windows: HashMap<ObjectId, SctkFlutterWindow>,
    pointers: HashMap<ObjectId, WlPointer>,
    startup_synchronizer: ImplicitWindowStartupSynchronizer,
    #[allow(dead_code)]
    plugins: Rc<RwLock<PluginRegistrar>>,
    mouse_cursor_handler: Arc<Mutex<SctkMouseCursorHandler>>,
    vsync_handler: Arc<Mutex<SctkVsyncHandler>>,
    #[allow(dead_code)]
    async_scheduler: Scheduler<SctkAsyncResult>,
}

impl SctkApplication {
    pub fn new(attributes: ApplicationAttributes) -> Result<Self, SctkApplicationCreateError> {
        let conn = Connection::connect_to_env()?;
        let (globals, event_queue) = registry_queue_init(&conn)?;
        let qh = event_queue.handle();

        let event_loop: EventLoop<SctkApplicationState> = EventLoop::try_new()?;
        WaylandSource::new(conn.clone(), event_queue).insert(event_loop.handle())?;

        let (async_executor, async_scheduler) = calloop::futures::executor::<SctkAsyncResult>()?;
        event_loop.handle().insert_source(
            async_executor,
            |event, _metadata, _state| match event {
                Ok(_) => {} // no-op
                Err(err) => error!("sctk async error: {:?}", err),
            },
        )?;

        let registry_state = RegistryState::new(&globals);
        let output_state = OutputState::new(&globals, &qh);
        let seat_state = SeatState::new(&globals, &qh);
        let compositor_state = CompositorState::bind(&globals, &qh)?;
        let xdg_shell_state = XdgShell::bind(&globals, &qh)?;
        let shm_state = Shm::bind(&globals, &qh)?;

        let platform_task_handler = Arc::new(SctkPlatformTaskHandler::new(event_loop.get_signal()));
        let vsync_handler = Arc::new(Mutex::new(SctkVsyncHandler::new(qh.clone())));

        let engine = FlutterEngineBuilder::new()
            .with_platform_handler(platform_task_handler)
            .with_vsync_handler(vsync_handler.clone())
            .with_asset_path(attributes.assets_path.clone())
            .with_icu_data_path(attributes.icu_data_path.clone())
            .with_args(attributes.args.clone())
            .with_compositor_enabled(true)
            .build()?;

        let implicit_window = SctkFlutterWindow::new(
            engine.downgrade(),
            &qh,
            &compositor_state,
            &xdg_shell_state,
            vsync_handler.clone(),
            attributes,
        )?;

        engine.add_view(implicit_window.create_flutter_view());

        vsync_handler
            .lock()
            .init(engine.downgrade(), implicit_window.wl_surface());

        let noop_isolate_cb = || trace!("[isolate-plugin] isolate has been created");
        let platform_handler = Arc::new(Mutex::new(SctkPlatformHandler::new(
            implicit_window.xdg_toplevel(),
        )));
        let mouse_cursor_handler = Arc::new(Mutex::new(SctkMouseCursorHandler::new(conn.clone())));
        let text_input_handler = Arc::new(Mutex::new(SctkTextInputHandler::new()));

        let mut plugins = PluginRegistrar::new();
        plugins.add_plugin(&engine, IsolatePlugin::new(noop_isolate_cb));
        plugins.add_plugin(&engine, KeyEventPlugin::default());
        plugins.add_plugin(&engine, TextInputPlugin::new(text_input_handler.clone()));
        plugins.add_plugin(&engine, LifecyclePlugin::default());
        plugins.add_plugin(&engine, LocalizationPlugin::default());
        plugins.add_plugin(&engine, NavigationPlugin::default());
        plugins.add_plugin(&engine, PlatformPlugin::new(platform_handler));
        plugins.add_plugin(&engine, SettingsPlugin::default());
        plugins.add_plugin(&engine, SystemPlugin::default());
        plugins.add_plugin(
            &engine,
            MouseCursorPlugin::new(mouse_cursor_handler.clone()),
        );

        let state = SctkApplicationState {
            conn,
            loop_handle: event_loop.handle(),
            loop_signal: event_loop.get_signal(),
            windows: HashMap::from([(implicit_window.xdg_toplevel_id(), implicit_window)]),
            pointers: HashMap::new(),
            compositor_state,
            shm_state,
            registry_state,
            output_state,
            seat_state,
            engine,
            startup_synchronizer: ImplicitWindowStartupSynchronizer::new(),
            plugins: Rc::new(RwLock::new(plugins)),
            mouse_cursor_handler,
            vsync_handler,
            async_scheduler,
        };

        Ok(Self { event_loop, state })
    }

    pub fn run(mut self) -> Result<(), SctkApplicationRunError> {
        // The event loop needs to be started *prior* to running the engine (see
        // `FlutterEngineRun` comment in `embedder.h` for additional context).
        // Therefore, use an immediate timer source for starting the engine
        // once the event loop is running.
        //
        // https://github.com/flutter/engine/blob/7c2a56a44b414f2790af277783ec27181337a6d3/shell/platform/embedder/embedder.h#L2313-L2322
        self.state
            .loop_handle
            .insert_source(Timer::immediate(), |_event, _metadata, state| {
                state.engine.run().expect("Failed to run engine");

                state.maybe_send_startup_pending_configure();

                TimeoutAction::Drop
            })?;

        self.event_loop.run(None, &mut self.state, |state| {
            let next_task_timer = state
                .engine
                .execute_platform_tasks()
                .map(Timer::from_deadline);

            insert_timer_source(&state.loop_handle, next_task_timer);
        })?;

        Ok(())
    }
}

impl SctkApplicationState {
    fn find_window_by_surface_id_mut(
        &mut self,
        surface_id: ObjectId,
    ) -> Option<&mut SctkFlutterWindow> {
        self.windows.iter_mut().find_map(|(_key, val)| {
            if val.wl_surface_id() == surface_id {
                Some(val)
            } else {
                None
            }
        })
    }

    fn get_implicit_window_mut(&mut self) -> Option<&mut SctkFlutterWindow> {
        self.windows.iter_mut().last().map(|(_key, window)| window)
    }

    fn maybe_send_startup_pending_configure(&mut self) {
        self.startup_synchronizer.is_engine_running = true;

        self.notify_display_update();

        let Some((configure, serial)) = self.startup_synchronizer.pending_configure.take() else {
            return;
        };

        let conn = self.conn.clone();
        if let Some(window) = self.get_implicit_window_mut() {
            window.configure(&conn, configure, serial);
        };
    }

    /// Find the maximum refresh rate from the surface current outputs.
    fn get_surface_refresh_rate_in_mhz(&self, surface: &WlSurface) -> Option<i32> {
        let data = surface.data::<SurfaceData>()?;

        let refresh_rate = data
            .outputs()
            .filter_map(|output| {
                let info = self.output_state.info(&output)?;
                let current_mode = info.modes.iter().find(|mode| mode.current)?;
                Some(current_mode.refresh_rate)
            })
            .max()?;

        Some(refresh_rate)
    }

    fn get_surface_frame_interval_in_nanos(&self, surface: &WlSurface) -> Option<u64> {
        let refresh_rate = self.get_surface_refresh_rate_in_mhz(surface)? as u64;

        // Refresh rate could be zero if an output has no correct refresh rate,
        // such as a virtual output.
        if refresh_rate == 0 {
            return None;
        }

        Some(1_000_000_000_000 / refresh_rate)
    }

    fn notify_display_update(&self) {
        // Ignore display update events if the engine is not running. This
        // method will be called again once the engine is running to ensure the
        // display state is up-to-date on the engine side.
        if !self.startup_synchronizer.is_engine_running {
            return;
        }

        let output_state = &self.output_state;
        let displays: Vec<FlutterEngineDisplay> = output_state
            .outputs()
            .map(|output| {
                SctkOutput::new(output.id().protocol_id().into(), output_state.info(&output)).into()
            })
            .collect();

        trace!("notifying engine of display update: {:?}", displays);

        self.engine.notify_display_update(
            flutter_engine::ffi::FlutterEngineDisplaysUpdateType::Startup,
            displays,
        );
    }
}

delegate_compositor!(SctkApplicationState);
delegate_output!(SctkApplicationState);
delegate_shm!(SctkApplicationState);

delegate_xdg_shell!(SctkApplicationState);
delegate_xdg_window!(SctkApplicationState);

delegate_seat!(SctkApplicationState);
delegate_pointer!(SctkApplicationState);
delegate_keyboard!(SctkApplicationState);

delegate_registry!(SctkApplicationState);

impl ProvidesRegistryState for SctkApplicationState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

impl CompositorHandler for SctkApplicationState {
    fn scale_factor_changed(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &WlSurface,
        new_scale_factor: i32,
    ) {
        trace!(
            "[{}] scale factor changed: {}",
            surface.id(),
            new_scale_factor
        );

        let Some(window) = self.find_window_by_surface_id_mut(surface.id()) else {
            warn!(
                "[{}] ignoring `scale_factor_changed` event for unknown flutter window",
                surface.id()
            );
            return;
        };

        window.scale_factor_changed(conn, surface, new_scale_factor);
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &WlSurface,
        new_transform: Transform,
    ) {
        trace!(
            "[{}] transform changed: {}",
            surface.id(),
            u32::from(new_transform),
        );
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &WlSurface,
        time: u32,
    ) {
        let baton = self.vsync_handler.lock().load_pending_baton();
        trace!(
            "[{} baton: {} time: {}] frame callback",
            surface.id(),
            baton,
            time
        );

        let frame_interval = self
            .get_surface_frame_interval_in_nanos(surface)
            .unwrap_or(FRAME_INTERVAL_60_HZ_IN_NANOS);

        let (frame_start_time_nanos, frame_target_time_nanos) =
            get_flutter_frame_time_nanos(frame_interval);

        self.engine
            .on_vsync(baton, frame_start_time_nanos, frame_target_time_nanos);
    }

    fn surface_enter(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wayland_client::protocol::wl_surface::WlSurface,
        output: &wayland_client::protocol::wl_output::WlOutput,
    ) {
        trace!("[{}] entered {}", surface.id(), output.id());

        let Some(window) = self.find_window_by_surface_id_mut(surface.id()) else {
            warn!(
                "[{}] ignoring `surface_enter` event for unknown flutter window",
                surface.id()
            );
            return;
        };

        window.surface_outputs_changed(conn, surface);
    }

    fn surface_leave(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wayland_client::protocol::wl_surface::WlSurface,
        output: &wayland_client::protocol::wl_output::WlOutput,
    ) {
        trace!("[{}] left {}", surface.id(), output.id());

        let Some(window) = self.find_window_by_surface_id_mut(surface.id()) else {
            warn!(
                "[{}] ignoring `surface_leave` event for unknown flutter window",
                surface.id()
            );
            return;
        };

        window.surface_outputs_changed(conn, surface);
    }
}

impl ShmHandler for SctkApplicationState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

impl PointerHandler for SctkApplicationState {
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let Some(window) = self.find_window_by_surface_id_mut(event.surface.id()) else {
                warn!(
                    "[{}] ignoring pointer event for unknown flutter window",
                    event.surface.id()
                );
                continue;
            };

            window.pointer_event(conn, pointer, event);
        }
    }
}

impl KeyboardHandler for SctkApplicationState {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _surface: &WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
        // not implemented
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _surface: &WlSurface,
        _serial: u32,
    ) {
        // not implemented
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
        // not implemented
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
        // not implemented
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _layout: u32,
    ) {
        // not implemented
    }
}

impl SeatHandler for SctkApplicationState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat) {
        // not needed for current implementation
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat) {
        // not needed for current implementation
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            let surface = self.compositor_state.create_surface(qh);
            let themed_pointer = self
                .seat_state
                .get_pointer_with_theme(
                    qh,
                    &seat,
                    self.shm_state.wl_shm(),
                    surface,
                    ThemeSpec::default(),
                )
                .ok();

            let pointer = themed_pointer
                .as_ref()
                .map(|themed_pointer| themed_pointer.pointer().clone());

            if let Some(pointer) = pointer {
                self.pointers.insert(seat.id(), pointer);
            } else {
                error!("Failed to create themed wayland pointer");
                self.pointers.remove(&seat.id());
            }

            self.mouse_cursor_handler
                .lock()
                .set_themed_pointer(themed_pointer);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        seat: wayland_client::protocol::wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.pointers.remove(&seat.id());

            self.mouse_cursor_handler
                .lock()
                .remove_themed_pointer_for_seat(seat.id());
        }
    }
}

impl OutputHandler for SctkApplicationState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, output: WlOutput) {
        trace!("[{}] new output", output.id());

        self.notify_display_update();
    }

    fn update_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, output: WlOutput) {
        trace!("[{}] update output", output.id());

        self.notify_display_update();
    }

    fn output_destroyed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, output: WlOutput) {
        trace!("[{}] output destroyed", output.id());

        self.notify_display_update();
    }
}

impl WindowHandler for SctkApplicationState {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.loop_signal.stop();
    }

    fn configure(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        window: &Window,
        configure: WindowConfigure,
        serial: u32,
    ) {
        let xdg_toplevel_id = window.xdg_toplevel().id();
        trace!(
            "[{}] configure: {}x{}",
            xdg_toplevel_id,
            configure.new_size.0.map_or(0, |v| v.get()),
            configure.new_size.1.map_or(0, |v| v.get()),
        );

        let Some(window) = self.windows.get_mut(&xdg_toplevel_id) else {
            warn!(
                "[{}] ignoring `configure` event for unknown flutter window",
                xdg_toplevel_id,
            );
            return;
        };

        if self.startup_synchronizer.is_engine_running {
            window.configure(conn, configure, serial);
        } else {
            trace!("Skipped sending window metrics event because engine is not running yet");
            self.startup_synchronizer
                .set_pending_configure(configure, serial);
        }
    }
}

#[derive(Error, Debug)]
pub enum SctkApplicationCreateError {
    #[error(transparent)]
    CalloopError(#[from] calloop::Error),

    #[error(transparent)]
    CalloopInsertWaylandSourceError(
        #[from] calloop::InsertError<WaylandSource<SctkApplicationState>>,
    ),

    #[error(transparent)]
    CalloopInsertAsyncExecutorError(#[from] calloop::InsertError<Executor<SctkAsyncResult>>),

    #[error(transparent)]
    ConnectError(#[from] ConnectError),

    #[error(transparent)]
    GlobalError(#[from] GlobalError),

    #[error(transparent)]
    BindError(#[from] BindError),

    #[error(transparent)]
    SctkFlutterWindowCreateError(#[from] SctkFlutterWindowCreateError),

    #[error(transparent)]
    EngineCreateError(#[from] CreateError),
}

#[derive(Error, Debug)]
pub enum SctkApplicationRunError {
    #[error(transparent)]
    DispatchError(#[from] calloop::Error),

    #[error(transparent)]
    InsertError(#[from] calloop::InsertError<Timer>),
}

fn insert_timer_source<Data>(handle: &LoopHandle<'static, Data>, timer: Option<Timer>) {
    let Some(timer) = timer else {
        return;
    };

    handle
        .insert_source(timer, |_, _, _| TimeoutAction::Drop)
        .expect("Unable to insert timer source");
}

// Trying to send a `WindowMetricsEvent` before the engine is running results in
// a `Viewport metrics were invalid` [embedder error][0]. This could happen when
// the first `window.configure` event arrives before the engine is fully
// running.
//
// The `ImplicitWindowStartupSynchronizer` is used as a way to synchronize the
// engine startup events in order to make sure that the initial window metrics
// event is only sent once a) the engine is running and b) the first configure
// event has been received.
//
// TODO: Get rid of this hack once Flutter supports disabling the implicit view
// as part of the [multi-view embedder APIs][1].
//
// [0]: https://github.com/flutter/engine/blob/e76c956498841e1ab458577d3892003e553e4f3c/shell/platform/embedder/embedder.cc#L2173-L2174
// [1]: https://github.com/flutter/flutter/issues/144806
#[derive(Default)]
struct ImplicitWindowStartupSynchronizer {
    pending_configure: Option<(WindowConfigure, u32)>,
    is_engine_running: bool,
}

impl ImplicitWindowStartupSynchronizer {
    fn new() -> Self {
        Default::default()
    }

    fn set_pending_configure(&mut self, configure: WindowConfigure, serial: u32) {
        self.pending_configure = Some((configure, serial));
    }
}
