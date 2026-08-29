#![cfg(target_os = "linux")]

use std::cell::RefCell;
use std::ffi::{c_void, CStr, CString};
use std::os::raw::{c_char, c_int};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use gtk::gdk;
use gtk::glib;
use gtk::glib::translate::{Stash, ToGlibPtr};
use gtk::prelude::*;
use libmpv2_sys::{mpv_handle, mpv_render_context};

const GL_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE: u32 = 0x8CD0;
const GL_FRAMEBUFFER_ATTACHMENT_OBJECT_NAME: u32 = 0x8CD1;
const GL_RENDERBUFFER: u32 = 0x8D41;
const GL_RENDERBUFFER_WIDTH: u32 = 0x8D42;
const GL_RENDERBUFFER_HEIGHT: u32 = 0x8D43;
const GL_SCISSOR_TEST: u32 = 0x0C11;
const GL_DEPTH_TEST: u32 = 0x0B71;
const GL_ONE: u32 = 1;
const GL_ONE_MINUS_SRC_ALPHA: u32 = 0x0303;
const RTLD_DEFAULT: *mut c_void = std::ptr::null_mut();

extern "C" {
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn gdk_x11_display_get_xdisplay(display: *mut c_void) -> *mut c_void;
    fn gdk_wayland_display_get_wl_display(display: *mut c_void) -> *mut c_void;
}

#[derive(Copy, Clone, PartialEq)]
enum Backend {
    X11,
    Wayland,
}

#[derive(Copy, Clone)]
struct MpvHandlePtr(NonNull<mpv_handle>);
unsafe impl Send for MpvHandlePtr {}

struct ProcLookup {
    backend: Backend,
}

struct UpdateCallback {
    callback: Box<dyn Fn() + Send + 'static>,
}

struct LinuxRenderContext {
    ctx: *mut mpv_render_context,
    proc_ctx: *mut c_void,
    update_cb: *mut c_void,
}

unsafe impl Send for LinuxRenderContext {}

impl Drop for LinuxRenderContext {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            unsafe { libmpv2_sys::mpv_render_context_free(self.ctx) };
            self.ctx = std::ptr::null_mut();
        }
        if !self.update_cb.is_null() {
            unsafe {
                let callback: *mut UpdateCallback = self.update_cb as *mut UpdateCallback;
                drop(Box::from_raw(callback));
            }
            self.update_cb = std::ptr::null_mut();
        }
        if !self.proc_ctx.is_null() {
            unsafe {
                let proc_ctx: *mut ProcLookup = self.proc_ctx as *mut ProcLookup;
                drop(Box::from_raw(proc_ctx));
            }
            self.proc_ctx = std::ptr::null_mut();
        }
    }
}

impl LinuxRenderContext {
    fn render_to_fbo(&self, fbo: i32, width: i32, height: i32) -> Result<(), String> {
        let target = libmpv2_sys::mpv_opengl_fbo {
            fbo,
            w: width,
            h: height,
            internal_format: 0,
        };
        let flip_y: c_int = 1;
        let params = [
            libmpv2_sys::mpv_render_param {
                type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_FBO,
                data: &target as *const _ as *mut c_void,
            },
            libmpv2_sys::mpv_render_param {
                type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_FLIP_Y,
                data: &flip_y as *const _ as *mut c_void,
            },
            libmpv2_sys::mpv_render_param {
                type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        let err = unsafe {
            libmpv2_sys::mpv_render_context_render(self.ctx, params.as_ptr() as *mut _)
        };
        if err == 0 {
            Ok(())
        } else {
            Err(format!("mpv_render_context_render failed: {}", err))
        }
    }
}

struct Pending {
    mpv: MpvHandlePtr,
    backend: Backend,
    display_native: u64,
}

struct Embed {
    area: gtk::GLArea,
    overlay: gtk::Overlay,
    gtk_window: gtk::ApplicationWindow,
    vbox: gtk::Box,
    web_view: gtk::Widget,
}

thread_local! {
    static EMBED: RefCell<Option<Embed>> = const { RefCell::new(None) };
    static PENDING: RefCell<Option<Pending>> = const { RefCell::new(None) };
}

static REDRAW_PENDING: AtomicBool = AtomicBool::new(false);
static PROC_WAYLAND: AtomicBool = AtomicBool::new(false);
static FBO_WIDTH: AtomicI32 = AtomicI32::new(-1);
static FBO_HEIGHT: AtomicI32 = AtomicI32::new(-1);
static LAST_SURFACE: AtomicU64 = AtomicU64::new(0);
static FBO_ZERO_WARNED: AtomicBool = AtomicBool::new(false);

type GlProcLoader = unsafe extern "C" fn(*const c_char) -> *mut c_void;

fn symbol_as_loader(name: &[u8]) -> Option<GlProcLoader> {
    let sym = unsafe { dlsym(RTLD_DEFAULT, name.as_ptr() as *const c_char) };
    (!sym.is_null()).then(|| unsafe { std::mem::transmute(sym) })
}

fn proc_loader_for(backend: Backend) -> Option<GlProcLoader> {
    if let Some(epoxy) = symbol_as_loader(b"epoxy_get_proc_address\0") {
        return Some(epoxy);
    }
    let primary: &[u8] = if backend == Backend::Wayland {
        b"eglGetProcAddress\0"
    } else {
        b"glXGetProcAddressARB\0"
    };
    let sym = unsafe { dlsym(RTLD_DEFAULT, primary.as_ptr() as *const c_char) };
    if !sym.is_null() {
        return Some(unsafe { std::mem::transmute(sym) });
    }
    let fallback = b"eglGetProcAddress\0";
    let sym = unsafe { dlsym(RTLD_DEFAULT, fallback.as_ptr() as *const c_char) };
    if !sym.is_null() {
        return Some(unsafe { std::mem::transmute(sym) });
    }
    let glx = unsafe { dlsym(RTLD_DEFAULT, b"glXGetProcAddressARB\0".as_ptr() as *const c_char) };
    (!glx.is_null()).then(|| unsafe { std::mem::transmute(glx) })
}

fn proc_loader() -> Option<GlProcLoader> {
    let backend = if PROC_WAYLAND.load(Ordering::Relaxed) {
        Backend::Wayland
    } else {
        Backend::X11
    };
    proc_loader_for(backend)
}

fn lookup_gl_proc(backend: Backend, name: &str) -> *mut c_void {
    let cstr = match CString::new(name) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    if let Some(loader) = proc_loader_for(backend) {
        let ptr = unsafe { loader(cstr.as_ptr()) };
        if !ptr.is_null() {
            return ptr;
        }
    }
    unsafe { dlsym(RTLD_DEFAULT, cstr.as_ptr()) }
}

fn get_proc_address(_ctx: &(), name: &str) -> *mut c_void {
    let backend = if PROC_WAYLAND.load(Ordering::Relaxed) {
        Backend::Wayland
    } else {
        Backend::X11
    };
    lookup_gl_proc(backend, name)
}

fn resolve_gl<T>(name: &[u8]) -> Option<T> {
    let sym = unsafe { dlsym(RTLD_DEFAULT, name.as_ptr() as *const c_char) };
    if !sym.is_null() {
        return Some(unsafe { std::mem::transmute_copy(&sym) });
    }
    let f = proc_loader()?;
    let via = unsafe { f(name.as_ptr() as *const c_char) };
    if via.is_null() {
        return None;
    }
    Some(unsafe { std::mem::transmute_copy(&via) })
}

fn current_fbo() -> i32 {
    let getter: Option<unsafe extern "C" fn(u32, *mut c_int)> = resolve_gl(b"glGetIntegerv\0");
    match getter {
        Some(f) => {
            let mut id: c_int = 0;
            unsafe { f(GL_FRAMEBUFFER_BINDING, &mut id) };
            id
        }
        None => 0,
    }
}

fn query_fbo_dimensions() -> Option<(i32, i32)> {
    let get_attach: unsafe extern "C" fn(u32, u32, u32, *mut c_int) =
        resolve_gl(b"glGetFramebufferAttachmentParameteriv\0")?;
    let bind_rb: unsafe extern "C" fn(u32, u32) = resolve_gl(b"glBindRenderbuffer\0")?;
    let get_rb: unsafe extern "C" fn(u32, u32, *mut c_int) =
        resolve_gl(b"glGetRenderbufferParameteriv\0")?;

    let mut obj_type: c_int = 0;
    unsafe {
        get_attach(
            GL_FRAMEBUFFER,
            GL_COLOR_ATTACHMENT0,
            GL_FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE,
            &mut obj_type,
        );
    }
    if obj_type != GL_RENDERBUFFER as c_int {
        return None;
    }
    let mut rb_id: c_int = 0;
    unsafe {
        get_attach(
            GL_FRAMEBUFFER,
            GL_COLOR_ATTACHMENT0,
            GL_FRAMEBUFFER_ATTACHMENT_OBJECT_NAME,
            &mut rb_id,
        );
    }
    if rb_id <= 0 {
        return None;
    }
    let mut w: c_int = 0;
    let mut h: c_int = 0;
    unsafe {
        bind_rb(GL_RENDERBUFFER, rb_id as u32);
        get_rb(GL_RENDERBUFFER, GL_RENDERBUFFER_WIDTH, &mut w);
        get_rb(GL_RENDERBUFFER, GL_RENDERBUFFER_HEIGHT, &mut h);
        bind_rb(GL_RENDERBUFFER, 0);
    }
    (w > 0 && h > 0).then_some((w, h))
}

fn detect_backend() -> Backend {
    if let Some(display) = gdk::Display::default() {
        let name = display.type_().name();
        if name.contains("Wayland") {
            return Backend::Wayland;
        }
        if name.contains("X11") {
            return Backend::X11;
        }
    }
    match std::env::var("XDG_SESSION_TYPE") {
        Ok(v) if v.eq_ignore_ascii_case("wayland") => Backend::Wayland,
        _ => Backend::X11,
    }
}

fn native_display(backend: Backend) -> u64 {
    let Some(display) = gdk::Display::default() else {
        return 0;
    };
    let stash: Stash<'_, *mut gdk::ffi::GdkDisplay, gdk::Display> = display.to_glib_none();
    let raw = stash.0 as *mut c_void;
    if raw.is_null() {
        return 0;
    }
    let native = match backend {
        Backend::X11 => unsafe { gdk_x11_display_get_xdisplay(raw) },
        Backend::Wayland => unsafe { gdk_wayland_display_get_wl_display(raw) },
    };
    native as u64
}

pub fn configure_linux_graphics() {
    // WebKit only implements transparent backgrounds inside the accelerated
    // compositor (WebKit bug 192305), and with compositing off every layer the
    // player promotes via transform/opacity/position:fixed collapses into
    // document order, which is what puts the subtitle modal behind the seek
    // bar. Harbor must therefore NOT disable compositing. WebKit checks these
    // variables for presence and ignores the value, so "=0" cannot undo it;
    // only never setting it works. HARBOR_LINUX_LEGACY_GFX=1 restores the old
    // behaviour for A/B testing without a rebuild.
    if std::env::var("HARBOR_LINUX_LEGACY_GFX").as_deref() == Ok("1") {
        eprintln!("[harbor::mpv_linux] HARBOR_LINUX_LEGACY_GFX=1: disabling WebKit accelerated compositing (legacy behaviour)");
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
    }
    if !std::path::Path::new("/proc/driver/nvidia/version").exists() {
        return;
    }
    let wayland = std::env::var("XDG_SESSION_TYPE")
        .map(|v| v.eq_ignore_ascii_case("wayland"))
        .unwrap_or(false)
        || std::env::var("WAYLAND_DISPLAY")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
    if wayland && std::env::var("__NV_DISABLE_EXPLICIT_SYNC").is_err() {
        eprintln!("[harbor::mpv_linux] NVIDIA + Wayland detected; setting __NV_DISABLE_EXPLICIT_SYNC=1");
        std::env::set_var("__NV_DISABLE_EXPLICIT_SYNC", "1");
    }
}

pub fn prepare(mpv_ctx: NonNull<mpv_handle>) -> Result<(), String> {
    let backend = detect_backend();
    PROC_WAYLAND.store(backend == Backend::Wayland, Ordering::Relaxed);
    let display_native = native_display(backend);
    PENDING.with(|slot| {
        *slot.borrow_mut() = Some(Pending {
            mpv: MpvHandlePtr(mpv_ctx),
            backend,
            display_native,
        })
    });
    Ok(())
}

pub fn install(gtk_window: &gtk::ApplicationWindow, vbox: &gtk::Box) -> Result<(), String> {
    if let Some(stale) = EMBED.with(|slot| slot.borrow_mut().take()) {
        restore_webview(stale);
    }
    let pending = PENDING
        .with(|slot| slot.borrow_mut().take())
        .ok_or_else(|| "no pending mpv ctx for linux install".to_string())?;

    apply_rgba_visual(gtk_window);
    let web_view = vbox
        .children()
        .into_iter()
        .next()
        .ok_or_else(|| "no webview child in vbox".to_string())?;

    let overlay = gtk::Overlay::new();
    let area = gtk::GLArea::new();
    area.set_auto_render(false);
    area.set_use_es(false);
    area.set_has_depth_buffer(false);
    area.set_has_stencil_buffer(false);
    area.set_app_paintable(true);
    // GLArea fills the full overlay via GTK expand flags. This avoids
    // set_size_request which on Wayland constrains the window's minimum
    // size to the video resolution.
    area.set_hexpand(true);
    area.set_vexpand(true);

    gtk_window.remove(vbox);
    vbox.remove(&web_view);
    overlay.add(&area);
    overlay.add_overlay(&web_view);
    overlay.set_overlay_pass_through(&web_view, false);
    gtk_window.add(&overlay);

    if probe("HARBOR_LINUX_OPAQUE_WEBVIEW") {
        eprintln!("[harbor::mpv_linux] probe: webview left opaque; video will be hidden, ghosting result is what matters");
        set_webview_opaque(&web_view);
    } else {
        set_webview_transparent(&web_view);
    }
    overlay.show_all();

    let render_cell: Rc<RefCell<Option<LinuxRenderContext>>> = Rc::new(RefCell::new(None));
    let mpv = pending.mpv;
    let backend = pending.backend;
    let display_native = pending.display_native;

    // Invalidate cached FBO dimensions on GLArea resize so the next
    // render call re-queries the physical GPU dimensions. This avoids
    // synchronous GL queries on every frame.
    let area_clone = area.clone();
    area.connect_resize(move |_a, _w, _h| {
        FBO_WIDTH.store(-1, Ordering::Relaxed);
        FBO_HEIGHT.store(-1, Ordering::Relaxed);
        area_clone.queue_render();
    });

    area.connect_render(move |area, _ctx| {
        let mut slot = render_cell.borrow_mut();
        if slot.is_none() {
            match build_render_context(mpv, backend, display_native) {
                Ok(rc) => {
                    *slot = Some(rc);
                }
                Err(e) => {
                    eprintln!("[harbor::mpv_linux] render ctx init failed: {}", e);
                    return glib::Propagation::Proceed;
                }
            }
        }
        if let Some(rc) = slot.as_ref() {
            do_render(rc, area);
        }
        glib::Propagation::Stop
    });

    EMBED.with(|slot| {
        *slot.borrow_mut() = Some(Embed {
            area: area.clone(),
            overlay,
            gtk_window: gtk_window.clone(),
            vbox: vbox.clone(),
            web_view,
        })
    });

    area.queue_render();
    eprintln!("[harbor::mpv_linux] installed backend={}", backend_label(backend));
    Ok(())
}

fn build_render_context(
    mpv: MpvHandlePtr,
    backend: Backend,
    display_native: u64,
) -> Result<LinuxRenderContext, String> {
    let proc_ctx = Box::into_raw(Box::new(ProcLookup { backend }));
    let init_params = libmpv2_sys::mpv_opengl_init_params {
        get_proc_address: Some(gl_proc_address),
        get_proc_address_ctx: proc_ctx as *mut c_void,
    };
    let init_box = Box::into_raw(Box::new(init_params));

    let mut params: Vec<libmpv2_sys::mpv_render_param> = vec![
        libmpv2_sys::mpv_render_param {
            type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
            data: libmpv2_sys::MPV_RENDER_API_TYPE_OPENGL.as_ptr() as *mut c_void,
        },
        libmpv2_sys::mpv_render_param {
            type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
            data: init_box as *mut c_void,
        },
    ];
    if display_native != 0 {
        let ptr = display_native as *const c_void;
        match backend {
            Backend::X11 => params.push(libmpv2_sys::mpv_render_param {
                type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_X11_DISPLAY,
                data: ptr as *mut c_void,
            }),
            Backend::Wayland => params.push(libmpv2_sys::mpv_render_param {
                type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_WL_DISPLAY,
                data: ptr as *mut c_void,
            }),
        }
    }
    params.push(libmpv2_sys::mpv_render_param {
        type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
        data: std::ptr::null_mut(),
    });

    let context_ptr = Box::into_raw(Box::new(std::ptr::null_mut() as *mut mpv_render_context));
    let raw_params = Box::into_raw(params.into_boxed_slice()) as *mut libmpv2_sys::mpv_render_param;
    let err = unsafe {
        libmpv2_sys::mpv_render_context_create(context_ptr, mpv.0.as_ptr(), raw_params)
    };
    drop(unsafe { Box::from_raw(raw_params) });
    drop(unsafe { Box::from_raw(init_box) });
    let ctx = unsafe { *Box::from_raw(context_ptr) };
    if err != 0 {
        unsafe { drop(Box::from_raw(proc_ctx)); }
        return Err(format!("render init failed: {}", err));
    }

    let callback_box = Box::into_raw(Box::new(UpdateCallback {
        callback: Box::new(|| schedule_redraw()),
    }));
    unsafe {
        libmpv2_sys::mpv_render_context_set_update_callback(
            ctx,
            Some(render_update_callback),
            callback_box as *mut c_void,
        );
    }

    Ok(LinuxRenderContext {
        ctx,
        proc_ctx: proc_ctx as *mut c_void,
        update_cb: callback_box as *mut c_void,
    })
}

unsafe extern "C" fn gl_proc_address(
    _ctx: *mut c_void,
    name: *const c_char,
) -> *mut c_void {
    let lookup = unsafe { &*( _ctx as *const ProcLookup ) };
    let requested = unsafe { CStr::from_ptr(name) };
    lookup_gl_proc(lookup.backend, requested.to_str().unwrap_or_default())
}

unsafe extern "C" fn render_update_callback(ctx: *mut c_void) {
    let callback = unsafe { &*(ctx as *const UpdateCallback) };
    (callback.callback)();
}

fn do_render(rc: &LinuxRenderContext, area: &gtk::GLArea) {
    area.attach_buffers();
    let fbo = current_fbo();

    let mut w = FBO_WIDTH.load(Ordering::Relaxed);
    let mut h = FBO_HEIGHT.load(Ordering::Relaxed);
    if w <= 0 || h <= 0 {
        // Query physical FBO dimensions on first render or after resize.
        // This avoids synchronous GL queries on every frame; the result
        // is cached and only invalidated by the GLArea::resize signal.
        if let Some((mw, mh)) = query_fbo_dimensions() {
            w = mw;
            h = mh;
        } else {
            w = area.allocated_width().max(1);
            h = area.allocated_height().max(1);
        }
        FBO_WIDTH.store(w, Ordering::Relaxed);
        FBO_HEIGHT.store(h, Ordering::Relaxed);
    }

    let packed = ((w as u64) << 32) | (h as u32 as u64);
    if LAST_SURFACE.swap(packed, Ordering::Relaxed) != packed {
        eprintln!(
            "[harbor::mpv_linux] render surface {}x{} px",
            w, h,
        );
    }
    if fbo == 0 && !FBO_ZERO_WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("[harbor::mpv_linux] WARNING: GtkGLArea FBO query returned 0; mpv will render to the default framebuffer and the video region will stay BLACK. glGetIntegerv or the GL proc loader likely failed to resolve.");
    }
    if let Err(err) = rc.render_to_fbo(fbo, w, h) {
        eprintln!("[harbor::mpv_linux] render failed: {}", err);
    }
    restore_gdk_gl_state(w, h);
}

// mpv/render_gl.h:49-57 documents that mpv expects OpenGL standard defaults on
// entry and does NOT restore glViewport, glScissor, glBlendFuncSeparate or
// glClearColor when it returns. GDK sets those once in begin_paint and then
// relies on them to composite the overlay children (the WebView) on top of the
// GLArea in the same context, so leaving mpv's values behind corrupts every
// widget drawn after the video. Driver agnostic: this is a contract, not a
// vendor quirk.
fn restore_gdk_gl_state(w: i32, h: i32) {
    if let Some(f) = resolve_gl::<unsafe extern "C" fn(c_int, c_int, c_int, c_int)>(b"glViewport\0") {
        unsafe { f(0, 0, w.max(1), h.max(1)) };
    }
    if let Some(f) = resolve_gl::<unsafe extern "C" fn(c_int, c_int, c_int, c_int)>(b"glScissor\0") {
        unsafe { f(0, 0, w.max(1), h.max(1)) };
    }
    if let Some(f) = resolve_gl::<unsafe extern "C" fn(u32)>(b"glDisable\0") {
        unsafe {
            f(GL_SCISSOR_TEST);
            f(GL_DEPTH_TEST);
        }
    }
    if let Some(f) = resolve_gl::<unsafe extern "C" fn(u32, u32)>(b"glBlendFunc\0") {
        unsafe { f(GL_ONE, GL_ONE_MINUS_SRC_ALPHA) };
    }
    if let Some(f) = resolve_gl::<unsafe extern "C" fn(f32, f32, f32, f32)>(b"glClearColor\0") {
        unsafe { f(0.0, 0.0, 0.0, 0.0) };
    }
}

// On Wayland, the GLArea fills the full window via GTK expand flags,
// so geometry tracking is not needed. This function is kept for API
// compatibility but is a no-op on Linux.
pub fn resize_to(_css: crate::mpv::MpvGeometry) -> Result<(), String> {
    EMBED.with(|slot| {
        if let Some(embed) = slot.borrow().as_ref() {
            embed.area.queue_render();
        }
    });
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    let Some(embed) = EMBED.with(|slot| slot.borrow_mut().take()) else {
        return Ok(());
    };
    restore_webview(embed);
    eprintln!("[harbor::mpv_linux] uninstalled");
    Ok(())
}

fn restore_webview(embed: Embed) {
    set_webview_opaque(&embed.web_view);
    if let Some(parent) = embed.web_view.parent() {
        if let Some(container) = parent.downcast_ref::<gtk::Container>() {
            container.remove(&embed.web_view);
        }
    }
    if embed.overlay.parent().is_some() {
        if let Some(parent) = embed.overlay.parent() {
            if let Some(container) = parent.downcast_ref::<gtk::Container>() {
                container.remove(&embed.overlay);
            }
        }
    }
    if embed.web_view.parent().is_none() {
        embed.vbox.pack_start(&embed.web_view, true, true, 0);
        embed.web_view.show();
    }
    if embed.vbox.parent().is_none() {
        embed.gtk_window.add(&embed.vbox);
    }
    embed.vbox.show_all();
}

fn schedule_redraw() {
    if REDRAW_PENDING.swap(true, Ordering::AcqRel) {
        return;
    }
    glib::idle_add(|| {
        REDRAW_PENDING.store(false, Ordering::Release);
        EMBED.with(|slot| {
            if let Some(embed) = slot.borrow().as_ref() {
                embed.area.queue_render();
                // GTK3 only adds a GdkWindow to the update area when its
                // user_data matches the widget being invalidated, and the
                // WebView owns its own child GdkWindow, so an mpv frame never
                // damages the UI layer. HARBOR_LINUX_DAMAGE_UI=1 forces it, at
                // the cost of a full WebKit repaint per frame.
                if probe("HARBOR_LINUX_DAMAGE_UI") {
                    embed.web_view.queue_draw();
                }
            }
        });
        glib::ControlFlow::Break
    });
}

fn probe(name: &str) -> bool {
    std::env::var(name).as_deref() == Ok("1")
}

fn apply_rgba_visual(window: &gtk::ApplicationWindow) {
    if let Some(screen) = GtkWindowExt::screen(window) {
        if let Some(visual) = screen.rgba_visual() {
            window.set_visual(Some(&visual));
            // GTK3 ignores set_visual after the widget is realized, and install()
            // runs at first playback, so the call above is a no-op on an already
            // mapped Tauri window. set_app_paintable DOES apply, and it removes
            // GtkWindow's background painter. WebKitGTK documents app-paintable
            // as a precondition for a transparent webview background, so this
            // stays on by default; HARBOR_LINUX_NO_APP_PAINTABLE=1 turns it off
            // to test whether the missing per-frame clear is the ghosting cause.
            if probe("HARBOR_LINUX_NO_APP_PAINTABLE") {
                eprintln!("[harbor::mpv_linux] probe: leaving GtkWindow background painter enabled");
            } else {
                window.set_app_paintable(true);
            }
        }
    }
}

fn set_webview_transparent(web_view: &gtk::Widget) {
    web_view.set_app_paintable(true);
    set_webview_bg_alpha(web_view, 0.0);
}

fn set_webview_opaque(web_view: &gtk::Widget) {
    web_view.set_app_paintable(false);
    set_webview_bg_alpha(web_view, 1.0);
}

fn set_webview_bg_alpha(web_view: &gtk::Widget, alpha: f64) {
    let setter: Option<unsafe extern "C" fn(*mut c_void, *const GdkRgba)> =
        resolve_symbol(b"webkit_web_view_set_background_color\0");
    if let Some(f) = setter {
        let stash: Stash<'_, *mut gtk::ffi::GtkWidget, gtk::Widget> = web_view.to_glib_none();
        let ptr = stash.0 as *mut c_void;
        let color = GdkRgba {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha,
        };
        unsafe { f(ptr, &color) };
    }
}

fn resolve_symbol<T>(name: &[u8]) -> Option<T> {
    let sym = unsafe { dlsym(RTLD_DEFAULT, name.as_ptr() as *const c_char) };
    if sym.is_null() {
        return None;
    }
    Some(unsafe { std::mem::transmute_copy(&sym) })
}

#[repr(C)]
struct GdkRgba {
    red: f64,
    green: f64,
    blue: f64,
    alpha: f64,
}

fn backend_label(backend: Backend) -> &'static str {
    match backend {
        Backend::X11 => "x11",
        Backend::Wayland => "wayland",
    }
}
