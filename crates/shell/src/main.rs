//! The browser executable.
//!
//! Phase 0 scope: open a window, own the single wgpu device and vello
//! renderer, clear the window to a color each frame, survive resize and
//! DPI changes. `--smoke` renders one frame and exits, for CI.
//!
//! See plan/02-architecture.md, "Shell".

#![forbid(unsafe_code)]

use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use vello::kurbo::{Affine, Rect, RoundedRect};
use vello::peniko::{Color, Fill};
use vello::util::{RenderContext, RenderSurface};
use vello::wgpu;
use vello::{AaConfig, AaSupport, RenderParams, Renderer, RendererOptions, Scene};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// Window background. Chosen to be obviously "ours" while there is no content.
const CLEAR: Color = Color::from_rgb8(0x1e, 0x1e, 0x2e);
const ACCENT: Color = Color::from_rgb8(0x89, 0xb4, 0xfa);

struct Options {
    /// Render one frame, then exit with status 0. Used by CI.
    smoke: bool,
}

fn parse_args() -> Options {
    let mut smoke = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--smoke" => smoke = true,
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    Options { smoke }
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
    active: Option<Active>,
    /// Set when startup fails so the process can exit non-zero from `main`.
    failure: Option<anyhow::Error>,
}

impl App {
    fn new(options: Options) -> Self {
        Self {
            options,
            context: RenderContext::new(),
            active: None,
            failure: None,
        }
    }

    fn start(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let attributes = Window::default_attributes()
            .with_title("browser")
            .with_inner_size(LogicalSize::new(1024.0, 768.0));
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
        Ok(())
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

        build_placeholder_scene(&mut active.scene, width, height, scale);

        let handle = &self.context.devices[active.surface.dev_id];
        let frame = match active.surface.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                // Nothing to draw into this time; the next redraw retries.
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
                    base_color: CLEAR,
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
}

/// Phase 0 has no page to draw. This draws a rounded panel so that a working
/// renderer is visually distinguishable from a stuck clear color, and so the
/// smoke test exercises the full fill pipeline rather than only the clear.
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

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.active.is_some() {
            return;
        }
        if let Err(e) = self.start(event_loop) {
            self.failure = Some(e);
            event_loop.exit();
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

    let options = parse_args();
    let event_loop = EventLoop::new().context("create event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new(options);
    event_loop.run_app(&mut app).context("event loop")?;
    match app.failure.take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
