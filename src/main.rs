//! woomer – zoomer for Wayland, running as a wlr-layer-shell overlay.
//!
//! Instead of a regular toplevel window (which tiling compositors place in
//! their layout), this creates a layer-shell surface on the overlay layer,
//! bound to one output, covering it completely and grabbing the keyboard.
//! Rendering is plain OpenGL 3.3 through EGL (wayland-egl).

use std::{
    env,
    ffi::c_void,
    ops::{Add, AddAssign, Div, Mul, Sub, SubAssign},
    process,
    time::Instant,
};

use glow::HasContext;
use khronos_egl as egl;
use libwayshot::WayshotConnection;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{
            CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer,
        },
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_egl::WlEglSurface;
use wayland_protocols::wp::{
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

const SPOTLIGHT_TINT: [f32; 4] = [0.0, 0.0, 0.0, 190.0 / 255.0];
const VELOCITY_THRESHOLD: f32 = 15.0;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const SPOTLIGHT_ZOOM_CAP: f32 = 5.0; // new change test please
const ZOOM_MIN: f64 = 0.01;
const ZOOM_MAX: f64 = 200.0;

const VERTEX_SRC: &str = include_str!("../shaders/vertex.vs");
const PLAIN_FRAG_SRC: &str = include_str!("../shaders/plain.fs");
#[cfg(not(feature = "dev"))]
const SPOTLIGHT_FRAG_SRC: &str = include_str!("../shaders/spotlight.fs");

// ---------------------------------------------------------------------------
// Tiny 2D vector (replaces raylib's Vector2)
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Default, Debug)]
struct V2 {
    x: f32,
    y: f32,
}
impl V2 {
    fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
    fn length_sqr(self) -> f32 {
        self.x * self.x + self.y * self.y
    }
}
impl Add for V2 {
    type Output = V2;
    fn add(self, o: V2) -> V2 {
        V2::new(self.x + o.x, self.y + o.y)
    }
}
impl Sub for V2 {
    type Output = V2;
    fn sub(self, o: V2) -> V2 {
        V2::new(self.x - o.x, self.y - o.y)
    }
}
impl Mul<f32> for V2 {
    type Output = V2;
    fn mul(self, k: f32) -> V2 {
        V2::new(self.x * k, self.y * k)
    }
}
impl Div<f32> for V2 {
    type Output = V2;
    fn div(self, k: f32) -> V2 {
        V2::new(self.x / k, self.y / k)
    }
}
impl AddAssign for V2 {
    fn add_assign(&mut self, o: V2) {
        *self = *self + o;
    }
}
impl SubAssign for V2 {
    fn sub_assign(&mut self, o: V2) {
        *self = *self - o;
    }
}

#[derive(Copy, Clone, Debug)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    #[allow(dead_code)]
    h: f32,
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

struct Config {
    monitor: Option<String>,
    output: Option<String>,
    radius: f32,
    show_cursor: bool,
}

fn parse_args() -> Config {
    let mut args = env::args();
    let bin = args.next().unwrap();
    let mut cfg = Config {
        monitor: None,
        output: None,
        radius: 1.0,
        show_cursor: false,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--monitor" => {
                cfg.monitor = Some(args.next().unwrap_or_else(|| {
                    eprintln!("--monitor needs a value");
                    process::exit(1);
                }))
            }
            "--output" => {
                cfg.output = Some(args.next().unwrap_or_else(|| {
                    eprintln!("--output needs a value");
                    process::exit(1);
                }))
            }
            "--radius" => {
                let s = args.next().unwrap_or_else(|| {
                    eprintln!("--radius needs a value");
                    process::exit(1);
                });
                cfg.radius = s.parse::<f32>().unwrap_or_else(|_| {
                    eprintln!("--radius must be a valid number (e.g., 5 or 3.5)");
                    process::exit(1);
                });
            }
            "--show-cursor" | "-S" => cfg.show_cursor = true,
            _ => print_help_and_exit(&bin),
        }
    }
    cfg
}

fn print_help_and_exit(bin: &str) -> ! {
    eprintln!(
        "\
{bin}  – Wayland screen-zoom tool

USAGE:
    {bin} [OPTIONS]

OPTIONS:
    --monitor <name>     Monitor that the overlay will be shown on. Defaults to the first one
    --output <name>      Monitor that should be captured. Captures all monitors if not specified
    --radius <number>    Size of the vignette radius. Bigger number means bigger radius. Defaults to 1
    -S --show-cursor     Whether to show cursor in the capture. Defaults to false"
    );
    process::exit(0);
}

// ---------------------------------------------------------------------------
// OpenGL / EGL
// ---------------------------------------------------------------------------

struct Prog {
    id: glow::Program,
    target: Option<glow::UniformLocation>,
    zoom: Option<glow::UniformLocation>,
    viewport: Option<glow::UniformLocation>,
    tex_size: Option<glow::UniformLocation>,
    flip: Option<glow::UniformLocation>,
    mirror: Option<glow::UniformLocation>,
    texture0: Option<glow::UniformLocation>,
    col_diffuse: Option<glow::UniformLocation>,
    tint: Option<glow::UniformLocation>,
    cursor: Option<glow::UniformLocation>,
    radius: Option<glow::UniformLocation>,
}

impl Prog {
    unsafe fn new(gl: &glow::Context, vs: &str, fs: &str) -> Result<Prog, String> {
        let compile = |kind, src: &str| -> Result<glow::Shader, String> {
            let sh = gl.create_shader(kind)?;
            gl.shader_source(sh, src);
            gl.compile_shader(sh);
            if !gl.get_shader_compile_status(sh) {
                let log = gl.get_shader_info_log(sh);
                gl.delete_shader(sh);
                return Err(log);
            }
            Ok(sh)
        };
        let v = compile(glow::VERTEX_SHADER, vs)?;
        let f = compile(glow::FRAGMENT_SHADER, fs)?;
        let id = gl.create_program()?;
        gl.attach_shader(id, v);
        gl.attach_shader(id, f);
        gl.link_program(id);
        gl.delete_shader(v);
        gl.delete_shader(f);
        if !gl.get_program_link_status(id) {
            let log = gl.get_program_info_log(id);
            gl.delete_program(id);
            return Err(log);
        }
        let loc = |n: &str| gl.get_uniform_location(id, n);
        Ok(Prog {
            id,
            target: loc("target"),
            zoom: loc("zoom"),
            viewport: loc("viewport"),
            tex_size: loc("texSize"),
            flip: loc("flip"),
            mirror: loc("mirror"),
            texture0: loc("texture0"),
            col_diffuse: loc("colDiffuse"),
            tint: loc("spotlightTint"),
            cursor: loc("cursorPosition"),
            radius: loc("spotlightRadiusMultiplier"),
        })
    }
}

struct Gfx {
    egl: egl::DynamicInstance<egl::EGL1_4>,
    egl_display: egl::Display,
    egl_surface: egl::Surface,
    _egl_context: egl::Context,
    wl_egl: WlEglSurface,
    gl: glow::Context,
    vao: glow::VertexArray,
    texture: glow::Texture,
    plain: Prog,
    spot: Prog,
}

fn spotlight_source() -> String {
    #[cfg(feature = "dev")]
    {
        std::fs::read_to_string("shaders/spotlight.fs")
            .expect("Failed to read shaders/spotlight.fs")
    }
    #[cfg(not(feature = "dev"))]
    {
        SPOTLIGHT_FRAG_SRC.to_string()
    }
}

impl Gfx {
    fn new(
        conn: &Connection,
        surface: &wl_surface::WlSurface,
        size: (i32, i32),
        image: &(u32, u32, Vec<u8>),
    ) -> Result<Gfx, String> {
        let egl = unsafe { egl::DynamicInstance::<egl::EGL1_4>::load_required() }
            .map_err(|e| format!("failed to load libEGL: {e}"))?;
        let display_ptr = conn.backend().display_ptr() as *mut c_void;
        let egl_display = unsafe { egl.get_display(display_ptr) }.ok_or("eglGetDisplay failed")?;
        egl.initialize(egl_display).map_err(|e| e.to_string())?;
        egl.bind_api(egl::OPENGL_API).map_err(|e| e.to_string())?;

        let config = egl
            .choose_first_config(
                egl_display,
                &[
                    egl::SURFACE_TYPE,
                    egl::WINDOW_BIT,
                    egl::RENDERABLE_TYPE,
                    egl::OPENGL_BIT,
                    egl::RED_SIZE,
                    8,
                    egl::GREEN_SIZE,
                    8,
                    egl::BLUE_SIZE,
                    8,
                    egl::ALPHA_SIZE,
                    8,
                    egl::NONE,
                ],
            )
            .map_err(|e| e.to_string())?
            .ok_or("no suitable EGL config")?;
        let ctx = egl
            .create_context(
                egl_display,
                config,
                None,
                &[
                    egl::CONTEXT_MAJOR_VERSION,
                    3,
                    egl::CONTEXT_MINOR_VERSION,
                    3,
                    egl::CONTEXT_OPENGL_PROFILE_MASK,
                    egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
                    egl::NONE,
                ],
            )
            .map_err(|e| format!("failed to create an OpenGL 3.3 core context: {e}"))?;

        let wl_egl = WlEglSurface::new(surface.id(), size.0, size.1).map_err(|e| e.to_string())?;
        let egl_surface = unsafe {
            egl.create_window_surface(
                egl_display,
                config,
                wl_egl.ptr() as egl::NativeWindowType,
                None,
            )
        }
        .map_err(|e| e.to_string())?;
        egl.make_current(egl_display, Some(egl_surface), Some(egl_surface), Some(ctx))
            .map_err(|e| e.to_string())?;
        // We pace frames ourselves with wl_surface.frame; a blocking swap would fight that.
        egl.swap_interval(egl_display, 0)
            .map_err(|e| e.to_string())?;

        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name)
                    .map_or(std::ptr::null(), |p| p as *const c_void)
            })
        };

        unsafe {
            let plain = Prog::new(&gl, VERTEX_SRC, PLAIN_FRAG_SRC)
                .map_err(|e| format!("plain shader: {e}"))?;
            let spot = Prog::new(&gl, VERTEX_SRC, &spotlight_source())
                .map_err(|e| format!("spotlight shader: {e}"))?;

            // Unit quad as a triangle strip.
            let vao = gl.create_vertex_array()?;
            gl.bind_vertex_array(Some(vao));
            let vbo = gl.create_buffer()?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            let quad: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];
            let bytes: Vec<u8> = quad.iter().flat_map(|f| f.to_ne_bytes()).collect();
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STATIC_DRAW);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);

            // Screenshot texture. Nearest filtering matches raylib's default.
            let texture = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                image.0 as i32,
                image.1 as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                Some(&image.2),
            );
            for (p, v) in [
                (glow::TEXTURE_MIN_FILTER, glow::NEAREST),
                (glow::TEXTURE_MAG_FILTER, glow::NEAREST),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
            }
            gl.disable(glow::BLEND);

            Ok(Gfx {
                egl,
                egl_display,
                egl_surface,
                _egl_context: ctx,
                wl_egl,
                gl,
                vao,
                texture,
                plain,
                spot,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

struct App {
    conn: Connection,
    qh: QueueHandle<App>,
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    shm: Shm,
    layer: Option<LayerSurface>,

    // fractional scaling (optional protocols)
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,

    // input
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    themed_pointer: Option<ThemedPointer>,
    modifiers: Modifiers,
    mouse: V2, // logical (surface) coordinates
    prev_mouse_px: V2,
    left_down: bool,

    // geometry
    logical_size: (u32, u32),
    scale: f64,
    buf: (i32, i32), // buffer size in pixels

    // capture
    image: (u32, u32, Vec<u8>),
    region: Option<Rect>, // display output inside the screenshot, in texture pixels
    gfx: Option<Gfx>,

    target: V2,
    zoom: f32,
    delta_scale: f64,
    scale_pivot: V2,
    velocity: V2,
    radius_base: f32,
    radius_mult: f32,
    radius_delta: f64,
    mirror: bool,
    flip: bool,
    spotlight: bool,

    frame_pending: bool,
    last_frame: Instant,
    exit: bool,
}

impl App {
    fn mouse_px(&self) -> V2 {
        self.mouse * self.scale as f32
    }

    fn screen_to_world(&self, p: V2) -> V2 {
        p / self.zoom + self.target
    }

    /// Camera that shows the display output 1:1 (or, if the captured area doesn't
    /// contain it, the whole screenshot fitted to the screen).
    fn home(&self) -> (V2, f32) {
        let (bw, bh) = (self.buf.0 as f32, self.buf.1 as f32);
        match self.region {
            Some(r) => (V2::new(r.x, r.y), bw / r.w),
            None => {
                let (tw, th) = (self.image.0 as f32, self.image.1 as f32);
                (V2::new(0.0, 0.0), (bw / tw).min(bh / th))
            }
        }
    }

    fn reset_view(&mut self) {
        let (t, z) = self.home();
        self.target = t;
        self.zoom = z;
        self.flip = false;
        self.mirror = false;
    }

    fn surface(&self) -> &wl_surface::WlSurface {
        self.layer.as_ref().unwrap().wl_surface()
    }

    /// Recompute the buffer size from logical size + scale and resize the EGL window.
    fn apply_buffer_size(&mut self) {
        let (lw, lh) = self.logical_size;
        if lw == 0 || lh == 0 {
            return;
        }
        let pw = ((lw as f64 * self.scale).round() as i32).max(1);
        let ph = ((lh as f64 * self.scale).round() as i32).max(1);
        if let Some(vp) = &self.viewport {
            vp.set_destination(lw as i32, lh as i32);
        } else {
            self.surface()
                .set_buffer_scale(self.scale.round().max(1.0) as i32);
        }
        if let Some(g) = &self.gfx {
            g.wl_egl.resize(pw, ph, 0, 0);
        }
        if self.buf.0 > 0 && (pw, ph) != self.buf {
            // keep what is on screen the same size on screen
            self.zoom *= pw as f32 / self.buf.0 as f32;
        }
        self.buf = (pw, ph);
    }

    fn set_scale(&mut self, scale: f64) {
        if (scale - self.scale).abs() < 1e-6 {
            return;
        }
        self.scale = scale;
        self.apply_buffer_size();
        self.kick();
    }

    fn init_gfx(&mut self) {
        self.apply_buffer_size();
        match Gfx::new(&self.conn, self.surface(), self.buf, &self.image) {
            Ok(g) => self.gfx = Some(g),
            Err(e) => {
                eprintln!("woomer: failed to initialise OpenGL: {e}");
                process::exit(1);
            }
        }
        self.reset_view();
        self.radius_mult = self.radius_base;
        self.last_frame = Instant::now();
    }

    // ---- input helpers ---------------------------------------------------

    fn on_scroll(&mut self, amount: f32) {
        if amount == 0.0 {
            return;
        }
        match (self.spotlight, self.modifiers.shift) {
            (_, false) => self.delta_scale += amount as f64,
            (true, true) => self.radius_delta += (amount * self.radius_mult / 2.0) as f64,
            _ => {}
        }
        self.scale_pivot = self.mouse_px();
    }

    fn on_key(&mut self, sym: Keysym) {
        match sym {
            // Q and A: AZERTY/QWERTY. Esc as documented.
            Keysym::q | Keysym::Q | Keysym::a | Keysym::A | Keysym::Escape => self.exit = true,
            Keysym::f | Keysym::F => {
                self.spotlight = !self.spotlight;
                if self.spotlight {
                    self.radius_mult = self.radius_base * 3.0;
                    self.radius_delta = -15.0;
                }
            }
            Keysym::m | Keysym::M => {
                // Mirror the view horizontally while keeping what's on screen in place.
                let view_w = self.buf.0 as f32 / self.zoom;
                self.target.x = self.image.0 as f32 - self.target.x - view_w;
                self.mirror = !self.mirror;
            }
            // Mirror the view Vertical. This and M should give you 180 rortation
            Keysym::v | Keysym::V => {
                let view_h = self.buf.1 as f32 / self.zoom;
                self.target.y = self.image.1 as f32 - self.target.y - view_h;
                self.flip = !self.flip;
            }
            Keysym::_0 | Keysym::KP_0 => self.reset_view(),
            #[cfg(feature = "dev")]
            Keysym::r | Keysym::R => self.reload_shader(),
            Keysym::Control_L | Keysym::Control_R => {
                self.radius_mult = self.radius_base * 3.0;
                self.radius_delta = -15.0;
            }
            _ => {}
        }
    }

    #[cfg(feature = "dev")]
    fn reload_shader(&mut self) {
        if let Some(g) = &mut self.gfx {
            match unsafe { Prog::new(&g.gl, VERTEX_SRC, &spotlight_source()) } {
                Ok(p) => g.spot = p,
                Err(e) => eprintln!("woomer: shader reload failed: {e}"),
            }
        }
    }

    // ---- frame loop --------------------------------------------------------

    /// Draw now if no frame callback is outstanding; otherwise the pending callback
    /// will pick up the new state.
    fn kick(&mut self) {
        if self.gfx.is_some() && !self.frame_pending && !self.exit {
            self.render(1.0 / 60.0);
        }
    }

    /// Advance animations by `dt`. Returns true while something is still moving.
    fn update(&mut self, dt: f32) -> bool {
        let mut animating = false;
        let dt64 = dt as f64;

        if self.delta_scale.abs() > 0.5 {
            let p0 = self.scale_pivot / self.zoom;
            self.zoom = (self.zoom as f64 * (self.delta_scale * dt64).exp())
                .clamp(ZOOM_MIN, ZOOM_MAX) as f32;
            let p1 = self.scale_pivot / self.zoom;
            self.target += p0 - p1;
            self.delta_scale -= self.delta_scale * dt64 * 4.0;
            animating = true;
        }

        self.radius_mult =
            (self.radius_mult as f64 + self.radius_delta * dt64).clamp(0.3, 10.0) as f32;
        self.radius_delta -= self.radius_delta * dt64 * 8.0;
        if self.radius_delta.abs() > 0.01 {
            animating = true;
        }

        let mouse = self.mouse_px();
        if self.left_down {
            let delta = self.screen_to_world(self.prev_mouse_px) - self.screen_to_world(mouse);
            self.target += delta;
            self.velocity = delta / dt;
            animating = true;
        } else if self.velocity.length_sqr() > VELOCITY_THRESHOLD * VELOCITY_THRESHOLD {
            self.target += self.velocity * dt;
            self.velocity -= self.velocity * dt * 6.0 * self.zoom.clamp(0.6, 10.0);
            animating = true;
        }
        self.prev_mouse_px = mouse;
        animating
    }

    fn render(&mut self, dt: f32) {
        let animating = self.update(dt);
        if animating {
            let s = self.surface().clone();
            s.frame(&self.qh, s.clone());
            self.frame_pending = true;
        }
        self.draw();
        self.last_frame = Instant::now();
    }

    fn draw(&self) {
        let Some(g) = &self.gfx else { return };
        let gl = &g.gl;
        let (pw, ph) = (self.buf.0 as f32, self.buf.1 as f32);
        unsafe {
            gl.viewport(0, 0, self.buf.0, self.buf.1);
            if self.spotlight {
                let t = SPOTLIGHT_TINT;
                gl.clear_color(t[0], t[1], t[2], t[3]);
            } else {
                gl.clear_color(0.0, 0.0, 0.0, 0.0);
            }
            gl.clear(glow::COLOR_BUFFER_BIT);

            let p = if self.spotlight { &g.spot } else { &g.plain };
            gl.use_program(Some(p.id));
            gl.uniform_2_f32(p.target.as_ref(), self.target.x, self.target.y);
            gl.uniform_1_f32(p.zoom.as_ref(), self.zoom);
            gl.uniform_2_f32(p.viewport.as_ref(), pw, ph);
            gl.uniform_2_f32(
                p.tex_size.as_ref(),
                self.image.0 as f32,
                self.image.1 as f32,
            );
            gl.uniform_1_f32(p.mirror.as_ref(), if self.mirror { 1.0 } else { 0.0 });
            gl.uniform_1_f32(p.flip.as_ref(), if self.flip { 1.0 } else { 0.0 });
            gl.uniform_1_i32(p.texture0.as_ref(), 0);
            gl.uniform_4_f32(p.col_diffuse.as_ref(), 1.0, 1.0, 1.0, 1.0);
            if self.spotlight {
                let t = SPOTLIGHT_TINT;
                gl.uniform_4_f32(p.tint.as_ref(), t[0], t[1], t[2], t[3]);
                // gl_FragCoord has its origin at the bottom-left.
                let m = self.mouse_px();
                gl.uniform_2_f32(p.cursor.as_ref(), m.x, ph - m.y);
                let zoom_for_radius = SPOTLIGHT_ZOOM_CAP * (self.zoom / SPOTLIGHT_ZOOM_CAP).tanh(); // new
                gl.uniform_1_f32(
                    p.radius.as_ref(),
                    self.radius_mult * zoom_for_radius * self.scale as f32,
                );
            }
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(g.texture));
            gl.bind_vertex_array(Some(g.vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
        // Commits the surface.
        if let Err(e) = g.egl.swap_buffers(g.egl_display, g.egl_surface) {
            eprintln!("woomer: eglSwapBuffers failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Wayland handlers
// ---------------------------------------------------------------------------

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        // Only used when fractional-scale isn't available.
        if self.fractional.is_none() {
            self.set_scale(new_factor as f64);
        }
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.frame_pending = false;
        if self.gfx.is_none() || self.exit {
            return;
        }
        let dt = self.last_frame.elapsed().as_secs_f32().clamp(0.001, 0.05);
        self.render(dt);
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (w, h) = configure.new_size;
        if w == 0 || h == 0 {
            return;
        }
        self.logical_size = (w, h);
        if self.gfx.is_none() {
            self.init_gfx();
        } else {
            self.apply_buffer_size();
        }
        self.kick();
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard if self.keyboard.is_none() => {
                self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
            }
            Capability::Pointer if self.pointer.is_none() => {
                let cursor_surface = self.compositor.create_surface(qh);
                match self.seat_state.get_pointer_with_theme(
                    qh,
                    &seat,
                    self.shm.wl_shm(),
                    cursor_surface,
                    ThemeSpec::System,
                ) {
                    Ok(tp) => {
                        self.pointer = Some(tp.pointer().clone());
                        self.themed_pointer = Some(tp);
                    }
                    Err(_) => self.pointer = self.seat_state.get_pointer(qh, &seat).ok(),
                }
            }
            _ => {}
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard => {
                if let Some(k) = self.keyboard.take() {
                    k.release();
                }
            }
            Capability::Pointer => {
                self.themed_pointer = None;
                if let Some(p) = self.pointer.take() {
                    p.release();
                }
            }
            _ => {}
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.on_key(event.keysym);
        self.kick();
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: u32,
    ) {
        self.modifiers = modifiers;
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for ev in events {
            let pos = V2::new(ev.position.0 as f32, ev.position.1 as f32);
            match ev.kind {
                PointerEventKind::Enter { .. } => {
                    self.mouse = pos;
                    self.prev_mouse_px = self.mouse_px();
                    if let Some(tp) = &self.themed_pointer {
                        let _ = tp.set_cursor(conn, CursorIcon::Default);
                    }
                }
                PointerEventKind::Leave { .. } => {}
                PointerEventKind::Motion { .. } => self.mouse = pos,
                PointerEventKind::Press { button, .. } => match button {
                    BTN_LEFT => {
                        self.mouse = pos;
                        self.prev_mouse_px = self.mouse_px();
                        self.left_down = true;
                    }
                    BTN_RIGHT => self.exit = true,
                    _ => {}
                },
                PointerEventKind::Release { button, .. } => {
                    if button == BTN_LEFT {
                        self.left_down = false;
                    }
                }
                PointerEventKind::Axis { vertical, .. } => {
                    // Wayland: scrolling up is negative; one wheel notch == 1.0.
                    let amount = if vertical.discrete != 0 {
                        -vertical.discrete as f32
                    } else {
                        -(vertical.absolute as f32) / 10.0
                    };
                    self.on_scroll(amount);
                }
            }
        }
        self.kick();
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

// Protocols that SCTK doesn't wrap. None of these have events we care about,
// except the fractional scale.
impl Dispatch<WpViewporter, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: <WpViewporter as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<WpViewport, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpViewport,
        _: <WpViewport as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<WpFractionalScaleManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpFractionalScaleManagerV1,
        _: <WpFractionalScaleManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<WpFractionalScaleV1, ()> for App {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.set_scale(scale as f64 / 120.0);
        }
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_pointer!(App);
delegate_layer!(App);
delegate_registry!(App);

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    let cfg = parse_args();

    // 1. Capture the screen *before* our overlay exists.
    let wayshot = WayshotConnection::new().expect("Failed to connect to wayshot");
    let outputs = wayshot.get_all_outputs();
    if outputs.is_empty() {
        eprintln!("No Wayland outputs found.");
        process::exit(1);
    }
    let find = |name: &str| {
        outputs.iter().find(|o| o.name == name).unwrap_or_else(|| {
            eprintln!("Monitor '{name}' not found.");
            process::exit(1);
        })
    };
    let display_output = cfg.monitor.as_deref().map_or(&outputs[0], find);
    let selected_output = cfg.output.as_deref().map(find);

    let shot = match selected_output {
        Some(o) => wayshot.screenshot_single_output(o, cfg.show_cursor),
        None => wayshot.screenshot_all(cfg.show_cursor),
    }
    .expect("failed to take a screenshot")
    .to_rgba8();
    let (tex_w, tex_h) = shot.dimensions();
    let image = (tex_w, tex_h, shot.into_raw());

    // Where the display output sits inside the screenshot (in texture pixels).
    // The capture is made at the highest output scale, so derive the pixel/logical
    // ratio from the actual image instead of assuming one.
    let captured: Vec<_> = match selected_output {
        Some(o) => vec![o],
        None => outputs.iter().collect(),
    };
    let rect_of = |o: &libwayshot::output::OutputInfo| {
        let r = o.logical_region.inner;
        (
            r.position.x,
            r.position.y,
            r.size.width as i32,
            r.size.height as i32,
        )
    };
    let min_x = captured.iter().map(|o| rect_of(o).0).min().unwrap();
    let min_y = captured.iter().map(|o| rect_of(o).1).min().unwrap();
    let max_x = captured
        .iter()
        .map(|o| rect_of(o).0 + rect_of(o).2)
        .max()
        .unwrap();
    let max_y = captured
        .iter()
        .map(|o| rect_of(o).1 + rect_of(o).3)
        .max()
        .unwrap();
    let ratio = tex_w as f32 / (max_x - min_x) as f32;
    let (dx, dy, dw, dh) = rect_of(display_output);
    let inside = dx >= min_x && dy >= min_y && dx + dw <= max_x && dy + dh <= max_y;
    let region = inside.then(|| Rect {
        x: (dx - min_x) as f32 * ratio,
        y: (dy - min_y) as f32 * ratio,
        w: dw as f32 * ratio,
        h: dh as f32 * ratio,
    });

    // 2. Our own Wayland connection for the overlay.
    let conn = Connection::connect_to_env().expect("Failed to connect to the Wayland compositor");
    let (globals, mut queue) = registry_queue_init(&conn).expect("Failed to read Wayland globals");
    let qh = queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let layer_shell = LayerShell::bind(&globals, &qh).unwrap_or_else(|_| {
        eprintln!("This compositor doesn't support wlr-layer-shell, which woomer needs.");
        process::exit(1);
    });
    let shm = Shm::bind(&globals, &qh).expect("wl_shm not available");
    let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
    let fractional_manager: Option<WpFractionalScaleManagerV1> = globals.bind(&qh, 1..=1, ()).ok();

    let mut app = App {
        conn: conn.clone(),
        qh: qh.clone(),
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        shm,
        layer: None,
        viewport: None,
        fractional: None,
        keyboard: None,
        pointer: None,
        themed_pointer: None,
        modifiers: Modifiers::default(),
        mouse: V2::default(),
        prev_mouse_px: V2::default(),
        left_down: false,
        logical_size: (0, 0),
        scale: 1.0,
        buf: (0, 0),
        image,
        region,
        gfx: None,
        target: V2::default(),
        zoom: 1.0,
        delta_scale: 0.0,
        scale_pivot: V2::default(),
        velocity: V2::default(),
        radius_base: cfg.radius,
        radius_mult: cfg.radius,
        radius_delta: 0.0,
        flip: false,
        mirror: false,
        spotlight: false,
        frame_pending: false,
        last_frame: Instant::now(),
        exit: false,
    };

    // Let the output events arrive so we can find the wl_output by name.
    queue.roundtrip(&mut app).unwrap();
    queue.roundtrip(&mut app).unwrap();

    let wl_output = app
        .output_state
        .outputs()
        .find(|o| {
            app.output_state.info(o).is_some_and(|i| {
                i.name.as_deref() == Some(display_output.name.as_str())
                    || i.logical_position == Some((dx, dy)) && i.logical_size == Some((dw, dh))
            })
        })
        .unwrap_or_else(|| {
            eprintln!(
                "Couldn't find output '{}' on the overlay connection.",
                display_output.name
            );
            process::exit(1);
        });
    if let Some(info) = app.output_state.info(&wl_output) {
        app.scale = info.scale_factor.max(1) as f64;
    }

    // 3. The layer-shell surface: overlay layer, all four edges, ignore other
    //    surfaces' exclusive zones, exclusive keyboard focus.
    let surface = app.compositor.create_surface(&qh);
    if let (Some(vp), Some(fm)) = (&viewporter, &fractional_manager) {
        app.viewport = Some(vp.get_viewport(&surface, &qh, ()));
        app.fractional = Some(fm.get_fractional_scale(&surface, &qh, ()));
    }
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some("woomer"),
        Some(&wl_output),
    );
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.set_size(0, 0);
    layer.commit();
    app.layer = Some(layer);

    // 4. Event loop. Drawing happens from frame callbacks and input handlers.
    while !app.exit {
        queue
            .blocking_dispatch(&mut app)
            .expect("Wayland connection lost");
    }
}
