//! The browser executable.
//!
//! Phase 1 scope: open a window, own the wgpu device and vello renderer,
//! run one tab on its own thread, show the frames it paints, forward
//! resize, scroll and keyboard input. `--smoke` renders one frame and
//! exits; `--screenshot FILE` saves the page once it has loaded and exits.
//!
//! See plan/02-architecture.md, "Shell".

#![forbid(unsafe_code)]

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use browser_ipc_types::{Cursor as PageCursor, MouseButton as PageButton, ShellToTab, TabId, TabToShell, Viewport};
use browser_net::NetService;
use browser_tab::{TabHandle, TabOutput, spawn_tab};
use url::Url;
use vello::kurbo::{Affine, Rect, RoundedRect};
use vello::peniko::{Color, Fill};
use vello::util::{RenderContext, RenderSurface};
use vello::wgpu;
use vello::{AaConfig, AaSupport, RenderParams, Renderer, RendererOptions, Scene};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

/// Window background while there is no page yet.
const CLEAR: Color = Color::from_rgb8(0x1e, 0x1e, 0x2e);
const ACCENT: Color = Color::from_rgb8(0x89, 0xb4, 0xfa);
const DEFAULT_URL: &str = "https://example.com/";
const LINE_SCROLL_PX: f32 = 40.0;
/// In screenshot mode, give up waiting for resources after this long.
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(25);

struct Options {
    /// Render one frame, then exit with status 0. Used by CI.
    smoke: bool,
    /// Save the loaded page to this PNG and exit.
    screenshot: Option<PathBuf>,
    width: f64,
    height: f64,
    url: Option<Url>,
}

fn parse_args() -> Result<Options> {
    let mut opts = Options {
        smoke: false,
        screenshot: None,
        width: 1024.0,
        height: 768.0,
        url: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--smoke" => opts.smoke = true,
            "--screenshot" => {
                let path = args.next().context("--screenshot needs a file path")?;
                opts.screenshot = Some(PathBuf::from(path));
            }
            "--width" => {
                opts.width = args.next().context("--width needs a number")?.parse()?;
            }
            "--height" => {
                opts.height = args.next().context("--height needs a number")?.parse()?;
            }
            other if other.starts_with("--") => anyhow::bail!("unknown argument: {other}"),
            other => {
                let url = if other.contains("://") {
                    Url::parse(other)
                } else {
                    Url::parse(&format!("https://{other}"))
                }
                .with_context(|| format!("invalid url: {other}"))?;
                opts.url = Some(url);
            }
        }
    }
    Ok(opts)
}

/// Events from tab threads, delivered through winit's proxy.
#[derive(Debug)]
enum UserEvent {
    Tab(TabId, TabOutput),
}

/// Everything that exists only while a window is open.
struct Active {
    window: Arc<Window>,
    surface: RenderSurface<'static>,
    renderer: Renderer,
    scene: Scene,
    frames_rendered: u64,
}

struct App {
    options: Options,
    context: RenderContext,
    proxy: EventLoopProxy<UserEvent>,
    net: Option<Arc<NetService>>,
    active: Option<Active>,
    tab: Option<TabHandle>,
    /// Latest frame from the tab, in physical pixels.
    page: Option<Scene>,
    page_loading: bool,
    page_title: Option<String>,
    screenshot_deadline: Option<Instant>,
    /// Pointer position in logical pixels while it is over the window.
    cursor_pos: Option<(f32, f32)>,
    /// Set when startup fails so the process can exit non-zero from `main`.
    failure: Option<anyhow::Error>,
}

impl App {
    fn new(options: Options, proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            options,
            context: RenderContext::new(),
            proxy,
            net: None,
            active: None,
            tab: None,
            page: None,
            page_loading: false,
            page_title: None,
            screenshot_deadline: None,
            cursor_pos: None,
            failure: None,
        }
    }

    fn scale(&self) -> f32 {
        self.active.as_ref().map(|a| a.window.scale_factor()).unwrap_or(1.0) as f32
    }

    fn viewport(&self) -> Option<Viewport> {
        let active = self.active.as_ref()?;
        let size = active.window.inner_size();
        let scale = active.window.scale_factor() as f32;
        Some(Viewport {
            width: size.width as f32 / scale,
            height: size.height as f32 / scale,
            scale_factor: scale,
        })
    }

    fn start(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let attributes = Window::default_attributes()
            .with_title("browser")
            .with_inner_size(LogicalSize::new(self.options.width, self.options.height));
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .context("create window")?,
        );
        let size = window.inner_size();
        let surface = pollster::block_on(self.context.create_surface(
            window.clone(),
            size.width.max(1),
            size.height.max(1),
            wgpu::PresentMode::AutoVsync,
        ))
        .context("create render surface")?;

        let device = &self.context.devices[surface.dev_id].device;
        let renderer = Renderer::new(
            device,
            RendererOptions {
                antialiasing_support: AaSupport::area_only(),
                num_init_threads: NonZeroUsize::new(1),
                ..RendererOptions::default()
            },
        )
        .map_err(|e| anyhow::anyhow!("create vello renderer: {e}"))?;

        tracing::info!(
            adapter = %self.context.devices[surface.dev_id].adapter().get_info().name,
            backend = ?self.context.devices[surface.dev_id].adapter().get_info().backend,
            width = size.width,
            height = size.height,
            scale = window.scale_factor(),
            "window ready"
        );

        self.active = Some(Active {
            window,
            surface,
            renderer,
            scene: Scene::new(),
            frames_rendered: 0,
        });

        if !self.options.smoke {
            self.open_tab()?;
        }
        Ok(())
    }

    fn open_tab(&mut self) -> Result<()> {
        let net = match &self.net {
            Some(n) => n.clone(),
            None => {
                let n = Arc::new(NetService::new().context("network service")?);
                self.net = Some(n.clone());
                n
            }
        };
        let viewport = self.viewport().context("no window")?;
        let proxy = self.proxy.clone();
        let sink: browser_tab::OutputSink = Box::new(move |id, out| {
            let _ = proxy.send_event(UserEvent::Tab(id, out));
        });
        let tab = spawn_tab(TabId(1), net, viewport, sink);
        let url = self
            .options
            .url
            .clone()
            .unwrap_or_else(|| Url::parse(DEFAULT_URL).expect("static url"));
        tab.send(ShellToTab::Navigate { url });
        self.page_loading = true;
        self.tab = Some(tab);
        if self.options.screenshot.is_some() {
            self.screenshot_deadline = Some(Instant::now() + SCREENSHOT_TIMEOUT);
        }
        Ok(())
    }

    fn send_viewport(&self) {
        if let (Some(tab), Some(vp)) = (&self.tab, self.viewport()) {
            tab.send(ShellToTab::Resize(vp));
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if width == 0 || height == 0 {
            return;
        }
        self.context
            .resize_surface(&mut active.surface, width, height);
        active.window.request_redraw();
        self.send_viewport();
    }

    fn render(&mut self) -> Result<()> {
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        let width = active.surface.config.width;
        let height = active.surface.config.height;
        if width == 0 || height == 0 {
            return Ok(());
        }
        let scale = active.window.scale_factor();

        match &self.page {
            Some(page) => {
                active.scene.reset();
                active.scene.append(page, None);
            }
            None => build_placeholder_scene(&mut active.scene, width, height, scale),
        }

        let handle = &self.context.devices[active.surface.dev_id];
        let frame = match active.surface.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.context.configure_surface(&active.surface);
                active.window.request_redraw();
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                anyhow::bail!("surface validation error while acquiring frame");
            }
        };

        active
            .renderer
            .render_to_texture(
                &handle.device,
                &handle.queue,
                &active.scene,
                &active.surface.target_view,
                &RenderParams {
                    base_color: if self.page.is_some() { Color::WHITE } else { CLEAR },
                    width,
                    height,
                    antialiasing_method: AaConfig::Area,
                },
            )
            .map_err(|e| anyhow::anyhow!("vello render: {e}"))?;

        let mut encoder = handle
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("blit to surface"),
            });
        let frame_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        active.surface.blitter.copy(
            &handle.device,
            &mut encoder,
            &active.surface.target_view,
            &frame_view,
        );
        handle.queue.submit([encoder.finish()]);
        frame.present();
        active.frames_rendered += 1;
        Ok(())
    }

    fn scroll(&self, dx: f32, dy: f32) {
        if let Some(tab) = &self.tab {
            tab.send(ShellToTab::Scroll { dx, dy });
        }
    }

    fn update_title(&self) {
        let Some(active) = &self.active else { return };
        let mut title = self.page_title.clone().unwrap_or_default();
        if title.is_empty() {
            title = self.options.url.as_ref().map(|u| u.to_string()).unwrap_or_default();
        }
        let title = if title.is_empty() { "browser".to_owned() } else { format!("{title} - browser") };
        active.window.set_title(&title);
    }

    fn take_screenshot(&mut self, event_loop: &ActiveEventLoop) {
        // Taking the path out makes this run once even though the loop
        // delivers a few more events before it actually exits.
        let Some(path) = self.options.screenshot.take() else { return };
        self.screenshot_deadline = None;
        let Some(page) = &self.page else {
            tracing::warn!("no frame to screenshot");
            return;
        };
        let Some(active) = &self.active else { return };
        let width = active.surface.config.width;
        let height = active.surface.config.height;
        match browser_paint::render_offscreen(page, width, height, Color::WHITE) {
            Some(pixels) => match image::RgbaImage::from_raw(width, height, pixels) {
                Some(img) => match img.save(&path) {
                    Ok(()) => tracing::info!("screenshot written to {}", path.display()),
                    Err(e) => self.failure = Some(anyhow::anyhow!("write screenshot: {e}")),
                },
                None => self.failure = Some(anyhow::anyhow!("screenshot buffer size mismatch")),
            },
            None => self.failure = Some(anyhow::anyhow!("offscreen render failed")),
        }
        event_loop.exit();
    }
}

/// Drawn until the first page frame arrives.
fn build_placeholder_scene(scene: &mut Scene, width: u32, height: u32, scale: f64) {
    scene.reset();
    let w = f64::from(width);
    let h = f64::from(height);
    let inset = 24.0 * scale;
    let panel = RoundedRect::new(inset, inset, w - inset, h - inset, 12.0 * scale);
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        Color::from_rgb8(0x31, 0x32, 0x44),
        None,
        &panel,
    );
    let bar_h = 40.0 * scale;
    let bar = Rect::new(inset, inset, w - inset, inset + bar_h);
    scene.fill(Fill::NonZero, Affine::IDENTITY, ACCENT, None, &bar);
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.active.is_some() {
            return;
        }
        if let Err(e) = self.start(event_loop) {
            self.failure = Some(e);
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        let UserEvent::Tab(_id, out) = event;
        match out {
            TabOutput::Frame(scene) => {
                self.page = Some(scene);
                if let Some(active) = &self.active {
                    active.window.request_redraw();
                }
                if self.options.screenshot.is_some() && !self.page_loading {
                    self.take_screenshot(event_loop);
                }
            }
            TabOutput::Message(TabToShell::StateChanged {
                title, loading, url, ..
            }) => {
                self.page_title = title.or_else(|| Some(url.to_string()));
                let was_loading = self.page_loading;
                self.page_loading = loading;
                self.update_title();
                if was_loading && !loading && self.options.screenshot.is_some() && self.page.is_some() {
                    // The last frame painted may predate the final resource;
                    // wait for the frame that follows this state change.
                    self.screenshot_deadline = Some(Instant::now() + Duration::from_millis(500));
                }
            }
            TabOutput::Message(TabToShell::Cursor(cursor)) => {
                if let Some(active) = &self.active {
                    active.window.set_cursor(match cursor {
                        PageCursor::Default => CursorIcon::Default,
                        PageCursor::Pointer => CursorIcon::Pointer,
                        PageCursor::Text => CursorIcon::Text,
                    });
                }
            }
            TabOutput::Message(TabToShell::Crashed { message }) => {
                tracing::error!("tab crashed: {message}");
                self.page_title = Some(format!("Tab crashed: {message}"));
                self.update_title();
                if self.options.screenshot.is_some() {
                    self.failure = Some(anyhow::anyhow!("tab crashed: {message}"));
                    event_loop.exit();
                }
            }
            TabOutput::Message(TabToShell::Closed) => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(deadline) = self.screenshot_deadline {
            if Instant::now() >= deadline {
                self.take_screenshot(event_loop);
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => self.resize(size.width, size.height),
            WindowEvent::ScaleFactorChanged { .. } => {
                // winit follows this with a Resized carrying the new physical
                // size; the redraw there picks up the new scale factor.
                if let Some(active) = &self.active {
                    active.window.request_redraw();
                }
                self.send_viewport();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (-x * LINE_SCROLL_PX, -y * LINE_SCROLL_PX),
                    MouseScrollDelta::PixelDelta(p) => {
                        let scale = self.scale();
                        (-(p.x as f32) / scale, -(p.y as f32) / scale)
                    }
                };
                self.scroll(dx, dy);
            }
            WindowEvent::CursorMoved { position, .. } => {
                let scale = self.scale();
                let (x, y) = (position.x as f32 / scale, position.y as f32 / scale);
                self.cursor_pos = Some((x, y));
                if let Some(tab) = &self.tab {
                    tab.send(ShellToTab::MouseMove { x, y });
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor_pos = None;
                if let Some(tab) = &self.tab {
                    tab.send(ShellToTab::MouseLeave);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let (Some(tab), Some((x, y))) = (&self.tab, self.cursor_pos) {
                    let button = match button {
                        MouseButton::Left => PageButton::Left,
                        MouseButton::Right => PageButton::Right,
                        MouseButton::Middle => PageButton::Middle,
                        _ => PageButton::Other,
                    };
                    tab.send(match state {
                        ElementState::Pressed => ShellToTab::MouseDown { x, y, button },
                        ElementState::Released => ShellToTab::MouseUp { x, y, button },
                    });
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let vp_h = self.viewport().map(|v| v.height).unwrap_or(600.0);
                match event.logical_key {
                    Key::Named(NamedKey::ArrowDown) => self.scroll(0.0, LINE_SCROLL_PX),
                    Key::Named(NamedKey::ArrowUp) => self.scroll(0.0, -LINE_SCROLL_PX),
                    Key::Named(NamedKey::PageDown) | Key::Named(NamedKey::Space) => self.scroll(0.0, vp_h * 0.9),
                    Key::Named(NamedKey::PageUp) => self.scroll(0.0, -vp_h * 0.9),
                    Key::Named(NamedKey::Home) => self.scroll(0.0, -1.0e9),
                    Key::Named(NamedKey::End) => self.scroll(0.0, 1.0e9),
                    Key::Named(NamedKey::F5) => {
                        if let Some(tab) = &self.tab {
                            tab.send(ShellToTab::Reload);
                        }
                    }
                    Key::Named(NamedKey::BrowserBack) => {
                        if let Some(tab) = &self.tab {
                            tab.send(ShellToTab::GoBack);
                        }
                    }
                    Key::Named(NamedKey::BrowserForward) => {
                        if let Some(tab) = &self.tab {
                            tab.send(ShellToTab::GoForward);
                        }
                    }
                    _ => {}
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.render() {
                    self.failure = Some(e);
                    event_loop.exit();
                    return;
                }
                if self.options.smoke
                    && self.active.as_ref().is_some_and(|a| a.frames_rendered >= 1)
                {
                    tracing::info!("smoke frame rendered");
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn,naga=warn".into()),
        )
        .init();

    let options = parse_args()?;
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("create event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App::new(options, proxy);
    event_loop.run_app(&mut app).context("event loop")?;
    if let Some(tab) = app.tab.take() {
        tab.close();
    }
    match app.failure.take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
