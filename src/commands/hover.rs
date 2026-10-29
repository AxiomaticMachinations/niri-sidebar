use super::hide::toggle_visibility;
use super::listen::get_ctx;
use crate::NiriClient;
use crate::config::{Config, SidebarPosition};
use anyhow::{Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            EventLoop, LoopHandle, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        calloop_wayland_source::WaylandSource,
        client::{
            Connection, QueueHandle,
            globals::registry_queue_init,
            protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use std::time::Duration;

// Extra room around the sidebar so grazing its edge doesn't trigger a collapse
const SAFE_PAD: i32 = 8;

struct HoverConfig {
    position: SidebarPosition,
    zone_width: i32,
    collapse_delay: Duration,
}

struct Hover {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    compositor: CompositorState,
    layer_shell: LayerShell,
    pool: SlotPool,
    loop_handle: LoopHandle<'static, Hover>,
    cfg: HoverConfig,
    pointer: Option<wl_pointer::WlPointer>,
    hot_zone: LayerSurface,
    overlay: Option<LayerSurface>,
    // Thickness of the area (measured from the docked edge) the pointer may roam without collapsing
    safe_thickness: i32,
    collapse_timer: Option<RegistrationToken>,
}

pub fn spawn(config: &Config) {
    let cfg = HoverConfig {
        position: config.interaction.position,
        zone_width: config
            .interaction
            .hover_zone_width
            .unwrap_or(config.interaction.peek)
            .max(1),
        collapse_delay: Duration::from_millis(config.interaction.hover_collapse_delay_ms),
    };
    let margin = side_margin(config);
    let fallback = fallback_thickness(config) + margin + SAFE_PAD;
    std::thread::spawn(move || {
        if let Err(e) = run(cfg, fallback) {
            eprintln!("niri-sidebar: hover disabled: {e:#}");
        }
    });
}

fn side_margin(config: &Config) -> i32 {
    let m = &config.margins;
    match config.interaction.position {
        SidebarPosition::Left => m.left,
        SidebarPosition::Right => m.right,
        SidebarPosition::Top => m.top,
        SidebarPosition::Bottom => m.bottom,
    }
}

fn fallback_thickness(config: &Config) -> i32 {
    match config.interaction.position {
        SidebarPosition::Left | SidebarPosition::Right => config.geometry.width,
        SidebarPosition::Top | SidebarPosition::Bottom => config.geometry.height,
    }
}

fn is_vertical_edge(position: SidebarPosition) -> bool {
    matches!(position, SidebarPosition::Left | SidebarPosition::Right)
}

fn edge_anchor(position: SidebarPosition) -> Anchor {
    match position {
        SidebarPosition::Right => Anchor::TOP | Anchor::BOTTOM | Anchor::RIGHT,
        SidebarPosition::Left => Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT,
        SidebarPosition::Top => Anchor::LEFT | Anchor::RIGHT | Anchor::TOP,
        SidebarPosition::Bottom => Anchor::LEFT | Anchor::RIGHT | Anchor::BOTTOM,
    }
}

/// Input region of the overlay: everything except the band along the docked edge.
fn outside_rect(position: SidebarPosition, thickness: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
    match position {
        SidebarPosition::Right => (0, 0, (w - thickness).max(0), h),
        SidebarPosition::Left => (thickness, 0, (w - thickness).max(0), h),
        SidebarPosition::Bottom => (0, 0, w, (h - thickness).max(0)),
        SidebarPosition::Top => (0, thickness, w, (h - thickness).max(0)),
    }
}

fn run(cfg: HoverConfig, fallback_thickness: i32) -> Result<()> {
    let conn = Connection::connect_to_env().context("connecting to Wayland")?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor missing")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("wlr-layer-shell missing")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm missing")?;
    let pool = SlotPool::new(4096, &shm)?;

    let mut event_loop: EventLoop<Hover> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn, event_queue)
        .insert(loop_handle.clone())
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let hot_zone = new_layer(&compositor, &layer_shell, &qh, "niri-sidebar-hover");
    hot_zone.set_anchor(edge_anchor(cfg.position));
    if is_vertical_edge(cfg.position) {
        hot_zone.set_size(cfg.zone_width as u32, 0);
    } else {
        hot_zone.set_size(0, cfg.zone_width as u32);
    }
    hot_zone.commit();

    let mut hover = Hover {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        compositor,
        layer_shell,
        pool,
        loop_handle,
        cfg,
        pointer: None,
        hot_zone,
        overlay: None,
        safe_thickness: fallback_thickness,
        collapse_timer: None,
    };

    event_loop.run(None, &mut hover, |_| {})?;
    Ok(())
}

fn new_layer(
    compositor: &CompositorState,
    layer_shell: &LayerShell,
    qh: &QueueHandle<Hover>,
    namespace: &str,
) -> LayerSurface {
    let surface = compositor.create_surface(qh);
    let layer = layer_shell.create_layer_surface(
        qh,
        surface,
        Layer::Overlay,
        Some(namespace.to_string()),
        None,
    );
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_exclusive_zone(-1);
    layer
}

impl Hover {
    fn on_hot_zone_enter(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let (mut ctx, _lock) = get_ctx()?;
        if !ctx.state.is_hidden {
            return Ok(());
        }
        toggle_visibility(&mut ctx)?;

        let ids: Vec<u64> = ctx.state.windows.iter().map(|w| w.id).collect();
        let vertical = is_vertical_edge(self.cfg.position);
        let size = ctx
            .socket
            .get_windows()?
            .into_iter()
            .filter(|w| w.is_floating && ids.contains(&w.id))
            .map(|w| {
                let (tw, th) = w.layout.tile_size;
                if vertical { tw } else { th }
            })
            .fold(0.0_f64, f64::max);
        let margin = side_margin(&ctx.config);
        if size > 0.0 {
            let size = size.ceil() as i32;
            let edge_gap = if ctx.config.interaction.get_focus_peek() >= size {
                ctx.config.interaction.get_focus_peek() - size
            } else {
                margin
            };
            self.safe_thickness = size + edge_gap.max(margin) + SAFE_PAD;
        }

        if self.overlay.is_none() {
            let layer = new_layer(
                &self.compositor,
                &self.layer_shell,
                qh,
                "niri-sidebar-hover-overlay",
            );
            layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
            layer.set_size(0, 0);
            layer.commit();
            self.overlay = Some(layer);
        }
        Ok(())
    }

    fn collapse(&mut self) -> Result<()> {
        self.collapse_timer = None;
        self.overlay = None;
        let (mut ctx, _lock) = get_ctx()?;
        if !ctx.state.is_hidden {
            toggle_visibility(&mut ctx)?;
        }
        Ok(())
    }

    fn start_collapse_timer(&mut self) {
        if self.collapse_timer.is_some() {
            return;
        }
        let timer = Timer::from_duration(self.cfg.collapse_delay);
        self.collapse_timer = self
            .loop_handle
            .insert_source(timer, |_, _, hover| {
                if let Err(e) = hover.collapse() {
                    eprintln!("niri-sidebar: hover collapse failed: {e:#}");
                }
                TimeoutAction::Drop
            })
            .ok();
    }

    fn cancel_collapse_timer(&mut self) {
        if let Some(token) = self.collapse_timer.take() {
            self.loop_handle.remove(token);
        }
    }

    fn present_transparent(&mut self, layer: &LayerSurface, w: i32, h: i32) {
        let Ok((buffer, canvas)) = self
            .pool
            .create_buffer(w, h, w * 4, wl_shm::Format::Argb8888)
        else {
            return;
        };
        canvas.fill(0);
        layer.wl_surface().damage_buffer(0, 0, w, h);
        if buffer.attach_to(layer.wl_surface()).is_ok() {
            layer.commit();
        }
    }
}

impl LayerShellHandler for Hover {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if self.overlay.as_ref().is_some_and(|o| o == layer) {
            self.overlay = None;
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (w, h) = (configure.new_size.0 as i32, configure.new_size.1 as i32);
        if w == 0 || h == 0 {
            return;
        }
        if self.overlay.as_ref().is_some_and(|o| o == layer) {
            let (x, y, rw, rh) = outside_rect(self.cfg.position, self.safe_thickness, w, h);
            if let Ok(region) = Region::new(&self.compositor) {
                region.add(x, y, rw, rh);
                layer
                    .wl_surface()
                    .set_input_region(Some(region.wl_region()));
            }
        }
        self.present_transparent(layer, w, h);
    }
}

impl PointerHandler for Hover {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let on_hot_zone = &event.surface == self.hot_zone.wl_surface();
            let on_overlay = self
                .overlay
                .as_ref()
                .is_some_and(|o| &event.surface == o.wl_surface());
            match event.kind {
                PointerEventKind::Enter { .. } if on_hot_zone => {
                    if let Err(e) = self.on_hot_zone_enter(qh) {
                        eprintln!("niri-sidebar: hover expand failed: {e:#}");
                    }
                }
                PointerEventKind::Enter { .. } if on_overlay => self.start_collapse_timer(),
                PointerEventKind::Leave { .. } if on_overlay => self.cancel_collapse_timer(),
                _ => {}
            }
        }
    }
}

impl SeatHandler for Hover {
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
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl CompositorHandler for Hover {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
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

impl OutputHandler for Hover {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for Hover {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(Hover);

impl ProvidesRegistryState for Hover {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(Hover);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_rect_excludes_docked_band() {
        assert_eq!(
            outside_rect(SidebarPosition::Right, 100, 1000, 500),
            (0, 0, 900, 500)
        );
        assert_eq!(
            outside_rect(SidebarPosition::Left, 100, 1000, 500),
            (100, 0, 900, 500)
        );
        assert_eq!(
            outside_rect(SidebarPosition::Bottom, 100, 1000, 500),
            (0, 0, 1000, 400)
        );
        assert_eq!(
            outside_rect(SidebarPosition::Top, 100, 1000, 500),
            (0, 100, 1000, 400)
        );
    }
}
