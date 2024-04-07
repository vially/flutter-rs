use std::{
    ffi::{c_void, CStr, CString},
    num::NonZeroU32,
    sync::{Arc, Mutex, RwLock, Weak},
};

use dpi::PhysicalSize;
use flutter_engine::{
    compositor::{
        CompositorCollectBackingStoreError, CompositorCreateBackingStoreError,
        CompositorPresentError, FlutterCompositorHandler,
    },
    ffi::{
        FlutterBackingStore, FlutterBackingStoreConfig, FlutterBackingStoreDescription,
        FlutterOpenGLBackingStore, FlutterOpenGLBackingStoreFramebuffer, FlutterOpenGLFramebuffer,
        FlutterPresentViewInfo,
    },
    tasks::TaskRunnerHandler,
};
use flutter_engine_api::FlutterOpenGLHandler;
use flutter_glutin::{
    context::{Context, ResourceContext},
    gl,
};
use flutter_plugins::{
    mousecursor::{MouseCursorError, MouseCursorHandler, SystemMouseCursor},
    platform::{AppSwitcherDescription, MimeError, PlatformHandler},
};
use log::{error, warn};
use smithay_client_toolkit::{
    reexports::{calloop::LoopSignal, protocols::xdg::shell::client::xdg_toplevel::XdgToplevel},
    seat::pointer::{CursorIcon, PointerData, PointerDataExt, ThemedPointer},
};
use wayland_backend::client::ObjectId;
use wayland_client::{Connection, Proxy};

use crate::window::SctkFlutterWindowInner;

const WINDOW_FRAMEBUFFER_ID: u32 = 0;

#[derive(Clone)]
pub(crate) struct SctkOpenGLHandler {
    window: Weak<SctkFlutterWindowInner>,
    context: Arc<Mutex<Context>>,
    resource_context: Arc<Mutex<ResourceContext>>,
    current_frame_size: Arc<RwLock<PhysicalSize<u32>>>,
}

impl SctkOpenGLHandler {
    pub(crate) fn new(
        window: Weak<SctkFlutterWindowInner>,
        context: Arc<Mutex<Context>>,
        resource_context: Arc<Mutex<ResourceContext>>,
    ) -> Self {
        Self {
            window,
            context,
            resource_context,
            current_frame_size: Default::default(),
        }
    }

    // Note: This callback is executed on the *platform* thread.
    pub(crate) fn resize(&self, size: PhysicalSize<NonZeroU32>) {
        self.context.lock().unwrap().resize(size);
    }

    fn load_current_frame_size(&self) -> PhysicalSize<u32> {
        *self.current_frame_size.read().unwrap()
    }
}

// Note: These callbacks are executed on the *render* thread.
impl FlutterOpenGLHandler for SctkOpenGLHandler {
    fn present(&self) -> bool {
        let frame_size = self.load_current_frame_size();
        // Check if this frame can be presented. This resizes the surface if a
        // resize is pending and |frame_size| matches the target size.
        if !self
            .window
            .upgrade()
            .unwrap()
            .on_frame_generated(frame_size)
        {
            return false;
        }

        if !self.context.lock().unwrap().present() {
            return false;
        }

        self.window.upgrade().unwrap().on_frame_presented();

        true
    }

    fn make_current(&self) -> bool {
        self.context.lock().unwrap().make_current()
    }

    fn clear_current(&self) -> bool {
        self.context.lock().unwrap().make_not_current()
    }

    fn fbo_with_frame_info_callback(&self, size: PhysicalSize<u32>) -> u32 {
        let mut current_frame_size = self.current_frame_size.write().unwrap();
        *current_frame_size = size;

        0
    }

    fn make_resource_current(&self) -> bool {
        self.resource_context.lock().unwrap().make_current()
    }

    fn gl_proc_resolver(&self, proc: &CStr) -> *mut c_void {
        self.context.lock().unwrap().get_proc_address(proc) as _
    }
}

#[derive(Clone)]
pub struct SctkCompositorHandler {
    window: Weak<SctkFlutterWindowInner>,
    context: Arc<Mutex<Context>>,
    gl: gl::Gl,
    format: u32,
}

impl SctkCompositorHandler {
    pub fn new(window: Weak<SctkFlutterWindowInner>, context: Arc<Mutex<Context>>) -> Self {
        context.lock().unwrap().make_current();

        let gl = gl::Gl::load_with(|symbol| {
            let proc = CString::new(symbol).unwrap();
            context.lock().unwrap().get_proc_address(proc.as_c_str())
        });

        context.lock().unwrap().make_not_current();

        Self {
            window,
            context,
            gl,
            // TODO: Use similar logic for detecting supported formats as the
            // Windows embedder:
            // https://github.com/flutter/engine/blob/a6acfa4/shell/platform/windows/compositor_opengl.cc#L23-L34
            format: gl::RGBA8,
        }
    }

    fn clear(&self) -> Result<(), CompositorPresentError> {
        let window = self.window.upgrade().unwrap();

        if !window.on_empty_frame_generated() {
            return Err(CompositorPresentError::PresentFailed(
                "Empty frame generated callback failed".into(),
            ));
        }

        if !self.context.lock().unwrap().make_current() {
            return Err(CompositorPresentError::PresentFailed(
                "Unable to make context current".into(),
            ));
        }

        unsafe {
            self.gl.ClearColor(0.0, 0.0, 0.0, 0.0);
            self.gl
                .Clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT | gl::STENCIL_BUFFER_BIT);
        };

        if !self.context.lock().unwrap().present() {
            return Err(CompositorPresentError::PresentFailed(
                "Present failed".into(),
            ));
        }

        window.on_frame_presented();
        Ok(())
    }
}

impl FlutterCompositorHandler for SctkCompositorHandler {
    fn present_view(&self, info: FlutterPresentViewInfo) -> Result<(), CompositorPresentError> {
        if info.layers.is_empty() {
            return self.clear();
        }

        // TODO: Support compositing layers and platform views.
        debug_assert_eq!(info.layers.len(), 1);
        let layer = info.layers.first().unwrap();
        debug_assert!(layer.offset.x == 0.0 && layer.offset.y == 0.0);

        let source_id = layer
            .content
            .get_opengl_backing_store_framebuffer_name()
            .ok_or(CompositorPresentError::PresentFailed(
                "Unable to retrieve framebuffer name from layer".into(),
            ))?;

        // TODO: Investigate if conversion to `u32` is correct
        let frame_size = PhysicalSize::<u32>::new(
            layer.size.width.round() as u32,
            layer.size.height.round() as u32,
        );

        let window = self.window.upgrade().unwrap();

        if !window.on_frame_generated(frame_size) {
            return Err(CompositorPresentError::PresentFailed(
                "Frame generated callback failed".into(),
            ));
        }

        if !self.context.lock().unwrap().make_current() {
            return Err(CompositorPresentError::PresentFailed(
                "Unable to make context current".into(),
            ));
        }

        unsafe {
            // Disable the scissor test as it can affect blit operations.
            // Prevents regressions like: https://github.com/flutter/flutter/issues/140828
            // See OpenGL specification version 4.6, section 18.3.1.
            self.gl.Disable(gl::SCISSOR_TEST);

            self.gl.BindFramebuffer(gl::READ_FRAMEBUFFER, source_id);
            self.gl
                .BindFramebuffer(gl::DRAW_FRAMEBUFFER, WINDOW_FRAMEBUFFER_ID);

            let width = layer.size.width.round() as i32;
            let height = layer.size.height.round() as i32;

            self.gl.BlitFramebuffer(
                0,                    // srcX0
                0,                    // srcY0
                width,                // srcX1
                height,               // srcY1
                0,                    // dstX0
                0,                    // dstY0
                width,                // dstX1
                height,               // dstY1
                gl::COLOR_BUFFER_BIT, // mask
                gl::NEAREST,          // filter
            );
        }

        if !self.context.lock().unwrap().present() {
            return Err(CompositorPresentError::PresentFailed(
                "Present failed".into(),
            ));
        }

        window.on_frame_presented();
        Ok(())
    }

    fn create_backing_store(
        &self,
        config: FlutterBackingStoreConfig,
    ) -> Result<FlutterBackingStore, CompositorCreateBackingStoreError> {
        let mut user_data = FlutterOpenGLBackingStoreFramebuffer::new();
        unsafe {
            self.gl.GenTextures(1, &mut user_data.texture_id);
            self.gl.GenFramebuffers(1, &mut user_data.framebuffer_id);

            self.gl
                .BindFramebuffer(gl::FRAMEBUFFER, user_data.framebuffer_id);
            self.gl.BindTexture(gl::TEXTURE_2D, user_data.texture_id);
            self.gl.TexParameteri(
                gl::TEXTURE_2D,
                gl::TEXTURE_MIN_FILTER,
                gl::NEAREST.try_into().unwrap(),
            );
            self.gl.TexParameteri(
                gl::TEXTURE_2D,
                gl::TEXTURE_MAG_FILTER,
                gl::NEAREST.try_into().unwrap(),
            );
            self.gl.TexParameteri(
                gl::TEXTURE_2D,
                gl::TEXTURE_WRAP_S,
                gl::CLAMP_TO_EDGE.try_into().unwrap(),
            );
            self.gl.TexParameteri(
                gl::TEXTURE_2D,
                gl::TEXTURE_WRAP_T,
                gl::CLAMP_TO_EDGE.try_into().unwrap(),
            );
            self.gl.TexImage2D(
                gl::TEXTURE_2D,
                0,
                gl::RGBA8.try_into().unwrap(),
                config.size.width.round() as i32,
                config.size.height.round() as i32,
                0,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                std::ptr::null(),
            );
            self.gl.BindTexture(gl::TEXTURE_2D, 0);
            self.gl.FramebufferTexture2D(
                gl::FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                user_data.texture_id,
                0,
            );
        };

        let framebuffer = FlutterOpenGLFramebuffer::new(self.format, user_data);
        let opengl_backing_store = FlutterOpenGLBackingStore::Framebuffer(framebuffer);
        let description = FlutterBackingStoreDescription::OpenGL(opengl_backing_store);
        let backing_store = FlutterBackingStore::new(description);

        Ok(backing_store)
    }

    fn collect_backing_store(
        &self,
        backing_store: FlutterBackingStore,
    ) -> Result<(), CompositorCollectBackingStoreError> {
        let FlutterBackingStoreDescription::OpenGL(opengl_backing_store) =
            backing_store.description
        else {
            return Err(CompositorCollectBackingStoreError::CollectFailed(
                "Only OpenGL backing stores are currently implemented".into(),
            ));
        };

        let FlutterOpenGLBackingStore::Framebuffer(mut framebuffer) = opengl_backing_store else {
            return Err(CompositorCollectBackingStoreError::CollectFailed(
                "Only OpenGL framebuffer backing stores are currently implemented".into(),
            ));
        };

        unsafe {
            self.gl
                .DeleteFramebuffers(1, &framebuffer.user_data.framebuffer_id);
            self.gl.DeleteTextures(1, &framebuffer.user_data.texture_id);
        }

        framebuffer.drop_raw_user_data();

        Ok(())
    }
}

pub struct SctkPlatformTaskHandler {
    signal: LoopSignal,
}

impl SctkPlatformTaskHandler {
    pub fn new(signal: LoopSignal) -> Self {
        Self { signal }
    }
}

impl TaskRunnerHandler for SctkPlatformTaskHandler {
    fn wake(&self) {
        self.signal.wakeup();
    }
}

// TODO(multi-view): Add support for multi-view once the `flutter/platform`
// plugin supports it.
pub struct SctkPlatformHandler {
    implicit_xdg_toplevel: XdgToplevel,
}

impl SctkPlatformHandler {
    pub fn new(xdg_toplevel: XdgToplevel) -> Self {
        Self {
            implicit_xdg_toplevel: xdg_toplevel,
        }
    }
}

impl PlatformHandler for SctkPlatformHandler {
    fn set_application_switcher_description(&mut self, description: AppSwitcherDescription) {
        self.implicit_xdg_toplevel.set_title(description.label);
    }

    fn set_clipboard_data(&mut self, _text: String) {
        error!(
            "Attempting to set the contents of the clipboard, which hasn't yet been implemented \
             on this platform."
        );
    }

    fn get_clipboard_data(&mut self, _mime: &str) -> Result<String, MimeError> {
        error!(
            "Attempting to get the contents of the clipboard, which hasn't yet been implemented \
             on this platform."
        );
        Ok("".to_string())
    }
}

pub struct SctkMouseCursorHandler {
    conn: Connection,
    themed_pointer: Option<ThemedPointer>,
}

impl SctkMouseCursorHandler {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn,
            themed_pointer: None,
        }
    }

    pub(crate) fn set_themed_pointer(&mut self, themed_pointer: Option<ThemedPointer>) {
        self.themed_pointer = themed_pointer;
    }

    pub(crate) fn remove_themed_pointer_for_seat(&mut self, seat_id: ObjectId) {
        let themed_pointer_belongs_to_seat = self
            .themed_pointer
            .as_ref()
            .and_then(|themed_pointer| {
                themed_pointer
                    .pointer()
                    .data::<PointerData>()
                    .map(|data| data.pointer_data().seat().id() == seat_id)
            })
            .unwrap_or_default();

        if themed_pointer_belongs_to_seat {
            self.themed_pointer = None;
        }
    }
}

impl MouseCursorHandler for SctkMouseCursorHandler {
    fn activate_system_cursor(&mut self, kind: SystemMouseCursor) -> Result<(), MouseCursorError> {
        let Some(themed_pointer) = self.themed_pointer.as_ref() else {
            warn!("[plugin: mousecursor] Unable to update cursor: themed pointer is empty");
            return Err(MouseCursorError);
        };

        let cursor: SctkMouseCursor = kind.into();

        match cursor.icon {
            Some(icon) => themed_pointer
                .set_cursor(&self.conn, icon)
                .or(Err(MouseCursorError)),
            None => themed_pointer.hide_cursor().or(Err(MouseCursorError)),
        }
    }
}

struct SctkMouseCursor {
    icon: Option<CursorIcon>,
}

impl From<SystemMouseCursor> for SctkMouseCursor {
    fn from(kind: SystemMouseCursor) -> Self {
        let icon = match kind {
            SystemMouseCursor::Click => Some(CursorIcon::Pointer),
            SystemMouseCursor::Alias => Some(CursorIcon::Alias),
            SystemMouseCursor::AllScroll => Some(CursorIcon::Default),
            SystemMouseCursor::Basic => Some(CursorIcon::Default),
            SystemMouseCursor::Cell => Some(CursorIcon::Cell),
            SystemMouseCursor::ContextMenu => Some(CursorIcon::ContextMenu),
            SystemMouseCursor::Copy => Some(CursorIcon::Copy),
            SystemMouseCursor::Disappearing => Some(CursorIcon::Default), // fallback
            SystemMouseCursor::Forbidden => Some(CursorIcon::NotAllowed),
            SystemMouseCursor::Grab => Some(CursorIcon::Grab),
            SystemMouseCursor::Grabbing => Some(CursorIcon::Grabbing),
            SystemMouseCursor::Help => Some(CursorIcon::Help),
            SystemMouseCursor::Move => Some(CursorIcon::Move),
            SystemMouseCursor::NoDrop => Some(CursorIcon::NoDrop),
            SystemMouseCursor::None => None,
            SystemMouseCursor::Precise => Some(CursorIcon::Crosshair),
            SystemMouseCursor::Progress => Some(CursorIcon::Progress),
            SystemMouseCursor::ResizeColumn => Some(CursorIcon::ColResize),
            SystemMouseCursor::ResizeDown => Some(CursorIcon::SResize),
            SystemMouseCursor::ResizeDownLeft => Some(CursorIcon::SwResize),
            SystemMouseCursor::ResizeDownRight => Some(CursorIcon::SeResize),
            SystemMouseCursor::ResizeLeft => Some(CursorIcon::WResize),
            SystemMouseCursor::ResizeLeftRight => Some(CursorIcon::EwResize),
            SystemMouseCursor::ResizeRight => Some(CursorIcon::EResize),
            SystemMouseCursor::ResizeRow => Some(CursorIcon::RowResize),
            SystemMouseCursor::ResizeUp => Some(CursorIcon::NResize),
            SystemMouseCursor::ResizeUpDown => Some(CursorIcon::NsResize),
            SystemMouseCursor::ResizeUpLeft => Some(CursorIcon::NwResize),
            SystemMouseCursor::ResizeUpLeftDownRight => Some(CursorIcon::NwseResize),
            SystemMouseCursor::ResizeUpRight => Some(CursorIcon::NeResize),
            SystemMouseCursor::ResizeUpRightDownLeft => Some(CursorIcon::NeswResize),
            SystemMouseCursor::Text => Some(CursorIcon::Text),
            SystemMouseCursor::VerticalText => Some(CursorIcon::VerticalText),
            SystemMouseCursor::Wait => Some(CursorIcon::Wait),
            SystemMouseCursor::ZoomIn => Some(CursorIcon::ZoomIn),
            SystemMouseCursor::ZoomOut => Some(CursorIcon::ZoomOut),
        };

        Self { icon }
    }
}
