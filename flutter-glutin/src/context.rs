use dpi::PhysicalSize;
use glutin::{
    api::egl,
    context::PossiblyCurrentContext,
    display::Display,
    prelude::{GlDisplay, NotCurrentGlContext, PossiblyCurrentGlContext},
    surface::{GlSurface, Surface, WindowSurface},
};
use std::{
    ffi::{c_void, CStr},
    num::NonZeroU32,
};

pub struct Context {
    display: Display,
    surface: Surface<WindowSurface>,
    context: Option<PossiblyCurrentContext>,
}

impl Context {
    pub fn new(
        display: Display,
        surface: Surface<WindowSurface>,
        context: PossiblyCurrentContext,
    ) -> Self {
        Self {
            display,
            surface,
            context: Some(context),
        }
    }

    pub fn make_current(&mut self) -> bool {
        match self.context.as_ref() {
            Some(ctx) => ctx.make_current(&self.surface).is_ok(),
            None => false,
        }
    }

    pub fn make_not_current(&mut self) -> bool {
        if let Some(ctx) = self.context.take() {
            if let Ok(ctx) = ctx.make_not_current() {
                self.context = Some(ctx.treat_as_possibly_current());
                return true;
            }
        }
        false
    }

    pub fn get_proc_address(&self, proc: &CStr) -> *const c_void {
        self.display.get_proc_address(proc)
    }

    pub fn resize(&mut self, size: PhysicalSize<NonZeroU32>) {
        if let Some(ctx) = self.context.as_ref() {
            self.surface.resize(ctx, size.width, size.height);
        }
    }

    pub fn present(&mut self) -> bool {
        match self.context.as_ref() {
            Some(ctx) => self.surface.swap_buffers(ctx).is_ok(),
            None => false,
        }
    }
}

// `Context` is only `Send` as long as it's used correctly by the engine (e.g.:
// `make_current`/`make_not_current` are *always* called in the correct order
// and on the correct thread). Therefore, just mark it as `Send` until a better
// solution is found.
//
// TODO: Find a solution that better leverages Rust's type system
unsafe impl Send for Context {}

pub struct ResourceContext {
    context: egl::context::PossiblyCurrentContext,
}

impl ResourceContext {
    pub fn new(context: egl::context::PossiblyCurrentContext) -> Self {
        Self { context }
    }

    pub fn make_current(&mut self) -> bool {
        self.context.make_current_surfaceless().is_ok()
    }
}

unsafe impl Send for ResourceContext {}
