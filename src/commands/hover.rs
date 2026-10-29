use super::hide::toggle_visibility;
use super::listen::get_ctx;
use super::reorder::reorder;
use crate::NiriClient;
use crate::config::{Config, HoverMode, SidebarPosition};
use crate::niri::{connect, sidebar_workspace};
use crate::state::{get_default_cache_dir, load_state, save_state};
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
const SYNC_INTERVAL: Duration = Duration::from_millis(500);

type Rect = (i32, i32, i32, i32);

struct HoverConfig {
    position: SidebarPosition,
    mode: HoverMode,
    zone_width: i32,
    collapse_delay: Duration,
}

struct Zone {
    // None for the single full-length zone used in `all` mode
    id: Option<u64>,
    layer: LayerSurface,
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
    qh: QueueHandle<Hover>,
    cfg: HoverConfig,
    pointer: Option<wl_pointer::WlPointer>,
    zones: Vec<Zone>,
    // (window id, start, length, visible sliver width) of each sidebar window along the docked edge
    zone_layout: Vec<(u64, i32, i32, i32)>,
    overlay: Option<LayerSurface>,
    // Thickness from the docked edge, and the edge span, the pointer may roam without collapsing
    safe_thickness: i32,
    safe_span: Option<(i32, i32)>,
    collapse_timer: Option<RegistrationToken>,
}

pub fn spawn(config: &Config) {
    let cfg = HoverConfig {
        position: config.interaction.position,
        mode: config.interaction.hover_mode,
        zone_width: config
            .interaction
            .hover_zone_width
            .unwrap_or(config.interaction.peek)
            .max(1),
        collapse_delay: Duration::from_millis(config.interaction.hover_collapse_delay_ms),
    };
    let fallback = fallback_thickness(config) + side_margin(config) + SAFE_PAD;
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
        SidebarPosition::Right => Anchor::RIGHT,
        SidebarPosition::Left => Anchor::LEFT,
        SidebarPosition::Top => Anchor::TOP,
        SidebarPosition::Bottom => Anchor::BOTTOM,
    }
}

/// Rect `thickness` deep along the docked edge, covering `span` = (start, length) along it.
fn edge_rect(position: SidebarPosition, thickness: i32, span: (i32, i32), w: i32, h: i32) -> Rect {
    let (start, len) = span;
    match position {
        SidebarPosition::Right => (w - thickness, start, thickness, len),
        SidebarPosition::Left => (0, start, thickness, len),
        SidebarPosition::Bottom => (start, h - thickness, len, thickness),
        SidebarPosition::Top => (start, 0, len, thickness),
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

    let mode = cfg.mode;
    let mut hover = Hover {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        compositor,
        layer_shell,
        pool,
        loop_handle: loop_handle.clone(),
        qh,
        cfg,
        pointer: None,
        zones: Vec::new(),
        zone_layout: Vec::new(),
        overlay: None,
        safe_thickness: fallback_thickness,
        safe_span: None,
        collapse_timer: None,
    };

    match mode {
        HoverMode::All => {
            let width = hover.cfg.zone_width;
            hover.make_zone(None, None, width)
        }
        HoverMode::Window => {
            // Zones follow the sidebar layout, so keep them in sync as windows come and go
            loop_handle
                .insert_source(Timer::from_duration(SYNC_INTERVAL), |_, _, hover| {
                    if let Err(e) = hover.sync_zones() {
                        eprintln!("niri-sidebar: hover sync failed: {e:#}");
                    }
                    TimeoutAction::ToDuration(SYNC_INTERVAL)
                })
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }

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
    /// Create an edge strip. `span` limits it to part of the edge; None spans the whole edge.
    fn make_zone(&mut self, id: Option<u64>, span: Option<(i32, i32)>, width: i32) {
        let layer = new_layer(
            &self.compositor,
            &self.layer_shell,
            &self.qh,
            "niri-sidebar-hover",
        );
        let pos = self.cfg.position;
        let zw = width as u32;
        let vertical = is_vertical_edge(pos);
        match span {
            None => {
                layer.set_anchor(match pos {
                    SidebarPosition::Right => Anchor::TOP | Anchor::BOTTOM | Anchor::RIGHT,
                    SidebarPosition::Left => Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT,
                    SidebarPosition::Top => Anchor::LEFT | Anchor::RIGHT | Anchor::TOP,
                    SidebarPosition::Bottom => Anchor::LEFT | Anchor::RIGHT | Anchor::BOTTOM,
                });
                if vertical {
                    layer.set_size(zw, 0);
                } else {
                    layer.set_size(0, zw);
                }
            }
            Some((start, len)) => {
                if vertical {
                    layer.set_anchor(Anchor::TOP | edge_anchor(pos));
                    layer.set_size(zw, len as u32);
                    layer.set_margin(start, 0, 0, 0);
                } else {
                    layer.set_anchor(Anchor::LEFT | edge_anchor(pos));
                    layer.set_size(len as u32, zw);
                    layer.set_margin(0, 0, 0, start);
                }
            }
        }
        layer.commit();
        self.zones.push(Zone { id, layer });
    }

    /// Window mode: rebuild the per-window strips whenever the hidden sidebar's layout changes.
    fn sync_zones(&mut self) -> Result<()> {
        let cache_dir = get_default_cache_dir()?;
        let state = load_state(&cache_dir)?;
        // Windows move while one is revealed; keep the strips stable until it collapses
        if state.revealed.is_some() && self.overlay.is_some() {
            return Ok(());
        }
        let mut layout = Vec::new();
        if state.is_hidden {
            let mut socket = connect()?;
            let all_windows = socket.get_windows()?;
            let sidebar_ids: Vec<u64> = state.windows.iter().map(|s| s.id).collect();
            let focused_ws = socket.get_active_workspace()?.id;
            let ws = sidebar_workspace(&all_windows, &sidebar_ids, focused_ws);
            let position = self.cfg.position;
            let vertical = is_vertical_edge(position);
            let (sw, sh) = socket.get_workspace_dimensions(ws)?;
            for w in all_windows {
                let (Some(pos), true) = (
                    w.layout.tile_pos_in_workspace_view,
                    // A focused window is already shown; a strip over it would block its input
                    w.is_floating
                        && !w.is_focused
                        && w.workspace_id == Some(ws)
                        && state.windows.iter().any(|s| s.id == w.id),
                ) else {
                    continue;
                };
                let (tw, th) = w.layout.tile_size;
                let (start, len) = if vertical { (pos.1, th) } else { (pos.0, tw) };
                // Niri keeps part of a floating window on screen, so measure the real sliver
                let visible = match position {
                    SidebarPosition::Right => sw as f64 - pos.0,
                    SidebarPosition::Left => pos.0 + tw,
                    SidebarPosition::Bottom => sh as f64 - pos.1,
                    SidebarPosition::Top => pos.1 + th,
                };
                layout.push((
                    w.id,
                    start.round() as i32,
                    len.round() as i32,
                    (visible.round() as i32).max(1),
                ));
            }
            layout.sort_unstable();
        }
        if layout == self.zone_layout {
            return Ok(());
        }
        self.overlay = None;
        self.cancel_collapse_timer();
        self.zones.clear();
        for &(id, start, len, width) in &layout {
            self.make_zone(Some(id), Some((start, len)), width);
        }
        self.zone_layout = layout;
        if !state.is_hidden && state.revealed.is_some() {
            let (mut ctx, _lock) = get_ctx()?;
            ctx.state.revealed = None;
            save_state(&ctx.state, &ctx.cache_dir)?;
        }
        Ok(())
    }

    fn on_zone_enter(&mut self, id: Option<u64>) -> Result<()> {
        let (mut ctx, _lock) = get_ctx()?;
        if !ctx.state.is_hidden {
            return Ok(());
        }
        let vertical = is_vertical_edge(self.cfg.position);
        let sidebar_ids: Vec<u64> = ctx.state.windows.iter().map(|w| w.id).collect();
        let windows = ctx.socket.get_windows()?;

        match id {
            None => toggle_visibility(&mut ctx)?,
            Some(id) => {
                if ctx.state.revealed == Some(id) && self.overlay.is_some() {
                    return Ok(());
                }
                ctx.state.revealed = Some(id);
                save_state(&ctx.state, &ctx.cache_dir)?;
                reorder(&mut ctx)?;
            }
        }

        // Re-read after the move: sizes along the stacking axis don't change, so this is safe
        let windows = if id.is_some() {
            windows
        } else {
            ctx.socket.get_windows()?
        };
        let size = windows
            .iter()
            .filter(|w| w.is_floating && sidebar_ids.contains(&w.id))
            .filter(|w| id.is_none_or(|id| w.id == id))
            .map(|w| {
                let (tw, th) = w.layout.tile_size;
                if vertical { tw } else { th }
            })
            .fold(0.0_f64, f64::max);
        if size > 0.0 {
            let margin = side_margin(&ctx.config);
            let size = size.ceil() as i32;
            let focus_peek = ctx.config.interaction.get_focus_peek();
            let edge_gap = if focus_peek >= size {
                focus_peek - size
            } else {
                margin
            };
            self.safe_thickness = size + edge_gap.max(margin) + SAFE_PAD;
        }
        self.safe_span = id.and_then(|id| {
            self.zone_layout
                .iter()
                .find(|z| z.0 == id)
                .map(|&(_, start, len, _)| (start - SAFE_PAD, len + 2 * SAFE_PAD))
        });

        self.cancel_collapse_timer();
        let layer = new_layer(
            &self.compositor,
            &self.layer_shell,
            &self.qh,
            "niri-sidebar-hover-overlay",
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, 0);
        layer.commit();
        self.overlay = Some(layer);
        Ok(())
    }

    fn collapse(&mut self) -> Result<()> {
        self.collapse_timer = None;
        self.overlay = None;
        let (mut ctx, _lock) = get_ctx()?;
        match self.cfg.mode {
            HoverMode::All => {
                if !ctx.state.is_hidden {
                    toggle_visibility(&mut ctx)?;
                }
            }
            HoverMode::Window => {
                if ctx.state.revealed.take().is_some() {
                    save_state(&ctx.state, &ctx.cache_dir)?;
                    reorder(&mut ctx)?;
                }
            }
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

    /// Areas of the overlay that must not capture the pointer.
    fn exclusions(&self, w: i32, h: i32) -> Vec<Rect> {
        let pos = self.cfg.position;
        let full = if is_vertical_edge(pos) { h } else { w };
        let mut rects = vec![edge_rect(
            pos,
            self.safe_thickness,
            self.safe_span.unwrap_or((0, full)),
            w,
            h,
        )];
        // Neighbouring strips stay reachable so the reveal can move between windows
        for &(_, start, len, width) in &self.zone_layout {
            rects.push(edge_rect(pos, width, (start, len), w, h));
        }
        rects
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
            if let Ok(region) = Region::new(&self.compositor) {
                region.add(0, 0, w, h);
                for (x, y, rw, rh) in self.exclusions(w, h) {
                    region.subtract(x, y, rw, rh);
                }
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
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let zone_id = self
                .zones
                .iter()
                .find(|z| &event.surface == z.layer.wl_surface())
                .map(|z| z.id);
            let on_overlay = self
                .overlay
                .as_ref()
                .is_some_and(|o| &event.surface == o.wl_surface());
            match event.kind {
                PointerEventKind::Enter { .. } if zone_id.is_some() => {
                    if let Err(e) = self.on_zone_enter(zone_id.flatten()) {
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
    fn edge_rect_hugs_docked_edge() {
        let span = (40, 100);
        assert_eq!(
            edge_rect(SidebarPosition::Right, 30, span, 1000, 500),
            (970, 40, 30, 100)
        );
        assert_eq!(
            edge_rect(SidebarPosition::Left, 30, span, 1000, 500),
            (0, 40, 30, 100)
        );
        assert_eq!(
            edge_rect(SidebarPosition::Bottom, 30, span, 1000, 500),
            (40, 470, 100, 30)
        );
        assert_eq!(
            edge_rect(SidebarPosition::Top, 30, span, 1000, 500),
            (40, 0, 100, 30)
        );
    }
}
