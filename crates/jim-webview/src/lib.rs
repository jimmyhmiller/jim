//! `jim-webview` — a Chromium-backed web pane for jim.
//!
//! A `"webview"` pane is a real jim pane, not an overlay: CEF renders
//! windowless (off-screen) and each painted frame is copied into a `Sprite`
//! filling the pane's `content_root`, so a page is clipped by the pane's
//! `RenderLayers`, z-ordered with other panes, and nestable in a canvas. The
//! pane structure mirrors `jim-flame`, which solves the same shape of problem.
//!
//! # Why CEF and not Servo
//!
//! This started on Servo. It rendered correctly but could not resize: from a
//! pane resize to a correctly sized frame measured 270-666ms and occasionally
//! ~10 seconds, and the frame delivered in between was 100% blank white. CEF
//! turns the same resize around in ~83ms and never emits a blank frame.
//!
//! # Threading
//!
//! CEF's browser-process API is main-thread only, so the store is a `NonSend`
//! resource keyed by entity — the same pattern `jim-terminal` uses for its VT
//! runtime. `do_message_loop_work()` is pumped from a Bevy system, which is
//! what `external_message_pump` in the CEF settings expects.

use std::collections::HashMap;

use bevy::ecs::system::NonSendMarker;
use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use bevy::render::render_resource::Extent3d;
use bevy::sprite::Anchor;
use bevy::window::{PrimaryWindow, RequestRedraw};
use serde_json::Value;

use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::input::ButtonState;
use jim_pane::{
    pt_to_content_local, topmost_pane_at, FocusedPane, KeyboardOwner, PaneContentDragged,
    PaneContentHovered,
    PaneContentPressed, PaneContentReleased, PaneKindMarker, PaneKindSpec, PaneRect, PaneRegistry,
    PaneTag, PaneViewport, MARGIN, TITLE_H,
};

mod client;
mod surface;

use client::{Cmd, HostClient};

/// Stable kind id stored in pane snapshots and used as the registry key.
pub const PANE_KIND: &str = "webview";

/// Shown when a webview pane is spawned without a `url` in its config.
const DEFAULT_URL: &str = "https://servo.org";

/// Height of the browser toolbar (back/forward/reload + URL bar) reserved at
/// the top of the pane's content area, in logical pixels.
const TOOLBAR_H: f32 = 28.0;

/// Where each toolbar control sits, as x-ranges in logical pixels.
const BTN_BACK: (f32, f32) = (4.0, 28.0);
const BTN_FWD: (f32, f32) = (28.0, 52.0);
const BTN_RELOAD: (f32, f32) = (52.0, 76.0);
const URL_X: f32 = 82.0;

/// Fallback size if the pane has no `PaneRect` yet at spawn time.
const FALLBACK_SIZE: Vec2 = Vec2::new(900.0, 620.0);

/// How long after an interaction to keep pumping CEF every frame.
///
/// Not a throttle on the engine — CEF is always pumped when it has work. This
/// keeps jim's *reactive* loop awake through an interaction so scrolling and
/// resizing look continuous instead of advancing only when a wakeup fires.
const BUSY_TAIL_SECS: f32 = 0.4;

/// Device pixels per wheel notch for line-based (non-trackpad) wheels.
const SCROLL_LINE_PX: f32 = 120.0;

/// Trackpads report `Pixel` deltas of only a few units per event. Passing
/// those straight to CEF and truncating to i32 meant gentle scrolling rounded
/// to zero and the page never moved — which is why a mouse wheel (±120 per
/// notch) worked while a trackpad appeared dead on some pages.
const SCROLL_PIXEL_GAIN: f32 = 3.0;

/// Adds the `"webview"` pane kind. The app shell installs this via
/// `app.add_plugins(WebviewPlugin)`.
pub struct WebviewPlugin;

impl Plugin for WebviewPlugin {
    fn build(&self, app: &mut App) {
        app.init_non_send_resource::<WebviewStore>()
            .add_systems(Startup, register_webview_kind)
            .add_systems(
                Update,
                (
                    webview_track_resize,
                    webview_pump,
                    webview_on_hover,
                    webview_on_press,
                    webview_on_drag,
                    webview_on_release,
                    webview_on_wheel,
                    webview_on_keys,
                    webview_track_focus,
                )
                    .chain(),
            );
    }
}

fn register_webview_kind(mut registry: ResMut<PaneRegistry>) {
    registry.register(PaneKindSpec {
        kind: PANE_KIND,
        display_name: "Web",
        // Deliberately absent from the radial ring: a web pane is only useful
        // with a URL, and the radial can't ask for one. It lives in the
        // command palette instead ("Open URL: …" / "Web").
        radial_icon: None,
        default_size: Vec2::new(900.0, 620.0),
        spawn: webview_spawn_from_config,
        snapshot: webview_snapshot,
        on_close: Some(webview_on_close),
    });
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

struct PaneWebview {
    host: HostClient,
    image: Handle<Image>,
    sprite: Entity,
    /// Physical pixel size the pane wants CEF to render at.
    px: (u32, u32),
    /// Sub-pixel scroll remainder. CEF takes integers, so the fraction is
    /// carried to the next event instead of being truncated away.
    scroll_carry: Vec2,
    /// Entity of the URL bar text, updated on navigation.
    url_text: Entity,
    /// `Some` while the URL bar is being edited; keys go here, not the page.
    editing: Option<String>,
    scale_factor: f32,
    url: String,
}

/// Holds the CEF host plus one entry per live webview pane.
///
/// `!Send` because CEF's browser-process API is main-thread only.
#[derive(Default)]
pub struct WebviewStore {
    panes: HashMap<Entity, PaneWebview>,
    /// Pump every frame until this time — see [`BUSY_TAIL_SECS`].
    busy_until: f32,
}

impl WebviewStore {
    fn mark_busy(&mut self, now: f32) {
        self.busy_until = now + BUSY_TAIL_SECS;
    }
}

// ---------------------------------------------------------------------------
// Spawn / snapshot / close
// ---------------------------------------------------------------------------

fn webview_spawn_from_config(
    world: &mut World,
    entity: Entity,
    content_root: Entity,
    config: &Value,
) {
    // Accept a URL however it arrives. Agents reach for whatever command they
    // can discover, so `jimctl widget --kind webview -- <url>` (which sends the
    // URL as `command`) has to work as well as an explicit `url` config.
    // Silently falling back to the default is what made an agent report
    // success while the pane showed the wrong page.
    let url = url_from_config(config).unwrap_or_else(|| {
        if !config.is_null() {
            warn!(
                "[webview] no URL in the pane config, opening {DEFAULT_URL} instead. Got: {config}"
            );
        }
        DEFAULT_URL.to_string()
    });

    let scale_factor = primary_scale_factor(world);
    let size = world
        .get::<PaneRect>(entity)
        .map(|r| r.size)
        .unwrap_or(FALLBACK_SIZE);
    let (content_w, content_h) = content_size(size);
    // The page gets the content area minus the toolbar strip.
    let page_h = (content_h - TOOLBAR_H).max(1.0);
    let px = physical_size(content_w, page_h, scale_factor);

    let host = match HostClient::spawn(&url, content_w, page_h, scale_factor) {
        Ok(h) => h,
        Err(e) => {
            error!("[webview] could not start the CEF host process: {e}");
            return;
        }
    };
    info!("[webview] host process started for {url}");

    let image = world
        .resource_mut::<Assets<Image>>()
        .add(blank_image(px.0, px.1));

    let font = world.resource::<jim_pane::PaneFont>().0.clone();

    // Toolbar background.
    world.spawn((
        ChildOf(content_root),
        Sprite {
            color: Color::srgb(0.13, 0.14, 0.17),
            custom_size: Some(Vec2::new(content_w, TOOLBAR_H)),
            ..default()
        },
        Anchor::TOP_LEFT,
        Transform::from_xyz(0.0, 0.0, 0.01),
        Visibility::Inherited,
    ));

    // Back / forward / reload. Plain glyphs: the chrome font renders these,
    // and colour emoji would panic Bevy's rasterizer.
    for (glyph, (x0, _)) in [("\u{2039}", BTN_BACK), ("\u{203A}", BTN_FWD), ("\u{21BB}", BTN_RELOAD)] {
        world.spawn((
            ChildOf(content_root),
            Text2d::new(glyph),
            TextFont {
                font: font.clone().into(),
                font_size: bevy::text::FontSize::Px(16.0),
                ..default()
            },
            TextColor(Color::srgb(0.75, 0.78, 0.85)),
            Anchor::TOP_LEFT,
            Transform::from_xyz(x0 + 6.0, -4.0, 0.02),
            Visibility::Inherited,
        ));
    }

    let url_text = world
        .spawn((
            ChildOf(content_root),
            Text2d::new(url.clone()),
            TextFont {
                font: font.clone().into(),
                font_size: bevy::text::FontSize::Px(12.0),
                ..default()
            },
            TextColor(Color::srgb(0.62, 0.66, 0.74)),
            Anchor::TOP_LEFT,
            Transform::from_xyz(URL_X, -7.0, 0.02),
            Visibility::Inherited,
        ))
        .id();

    let sprite = world
        .spawn((
            ChildOf(content_root),
            Sprite {
                image: image.clone(),
                custom_size: Some(Vec2::new(content_w, page_h)),
                ..default()
            },
            // content_root's origin is the content top-left; the page sits
            // below the toolbar.
            Anchor::TOP_LEFT,
            Transform::from_xyz(0.0, -TOOLBAR_H, 0.0),
            Visibility::Inherited,
        ))
        .id();

    // A pane is normally focused the moment it opens, and `webview_track_focus`
    // only reacts to *changes* — so tell CEF now or the first click into a text
    // field on a fresh pane does nothing.
    let focused_now = world.resource::<FocusedPane>().0 == Some(entity);

    world
        .non_send_resource_mut::<WebviewStore>()
        .panes
        .insert(
            entity,
            PaneWebview {
                host,
                image,
                sprite,
                px,
                scroll_carry: Vec2::ZERO,
                url_text,
                editing: None,
                scale_factor,
                url,
            },
        );

    if focused_now {
        if let Some(pane) = world
            .non_send_resource_mut::<WebviewStore>()
            .panes
            .get_mut(&entity)
        {
            pane.host.send(Cmd::Focus(true));
        }
    }
}

fn webview_snapshot(world: &World, entity: Entity) -> Value {
    let url = world
        .non_send_resource::<WebviewStore>()
        .panes
        .get(&entity)
        .map(|p| p.url.clone())
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    serde_json::json!({ "url": url })
}

fn webview_on_close(world: &mut World, entity: Entity) {
    // Dropping the entry closes the browser and its renderer process. The CEF
    // host stays up: reopening a web pane is common and re-initializing CEF
    // is not supported within a process anyway.
    // Dropping the client kills the host process and removes its socket.
    world
        .non_send_resource_mut::<WebviewStore>()
        .panes
        .remove(&entity);
}

// ---------------------------------------------------------------------------
// Frame pump
// ---------------------------------------------------------------------------

fn webview_pump(
    _main_thread: NonSendMarker,
    mut store: NonSendMut<WebviewStore>,
    mut images: ResMut<Assets<Image>>,
    mut sprites: Query<&mut Sprite>,
    mut url_texts: Query<&mut Text2d>,
    mut redraw: MessageWriter<RequestRedraw>,
    time: Res<Time>,
) {
    let now = time.elapsed_secs();
    let busy = now < store.busy_until;
    if busy {
        redraw.write(RequestRedraw);
    }

    // Address changes reported by CEF (link clicks, redirects) — the URL bar
    // would otherwise show only what we asked for, not where we ended up.
    let entities: Vec<Entity> = store.panes.keys().copied().collect();
    for entity in entities {
        let (mut latest_url, editing, text_entity) = {
            let Some(pane) = store.panes.get_mut(&entity) else {
                continue;
            };
            let mut newest = None;
            while let Ok(u) = pane.host.urls.try_recv() {
                newest = Some(u);
            }
            if let Some(u) = &newest {
                pane.url = u.clone();
            }
            (newest, pane.editing.clone(), pane.url_text)
        };
        // While editing, show the buffer being typed rather than the page URL.
        if let Some(buf) = editing {
            latest_url = Some(buf);
        }
        if let Some(show) = latest_url {
            if let Ok(mut text) = url_texts.get_mut(text_entity) {
                **text = show;
            }
        }
    }

    for pane in store.panes.values_mut() {
        // Drain to the newest frame: if several arrived since the last tick,
        // only the last one is worth drawing.
        let mut newest = None;
        while let Ok(f) = pane.host.frames.try_recv() {
            newest = Some(f);
        }
        let Some(frame) = newest else { continue };

        let Some(pixels) = surface::read(frame.id) else {
            warn!("[webview] IOSurface {} vanished before we read it", frame.id);
            continue;
        };
        let (w, h) = (pixels.width, pixels.height);

        let Some(mut image) = images.get_mut(&pane.image) else {
            continue;
        };
        let size = image.texture_descriptor.size;
        if size.width != w || size.height != h {
            image.resize(Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            });
        }
        image.data = Some(pixels.bgra);

        // Present at 1:1 so a frame is never stretched to the pane and cannot
        // distort while the page is still reflowing.
        if let Ok(mut sprite) = sprites.get_mut(pane.sprite) {
            sprite.custom_size = Some(Vec2::new(
                w as f32 / pane.scale_factor,
                h as f32 / pane.scale_factor,
            ));
        }
        redraw.write(RequestRedraw);
    }
}

// ---------------------------------------------------------------------------
// Resize
// ---------------------------------------------------------------------------

fn webview_track_resize(
    _main_thread: NonSendMarker,
    time: Res<Time>,
    mut store: NonSendMut<WebviewStore>,
    rects: Query<&PaneRect>,
) {
    let now = time.elapsed_secs();
    let entities: Vec<Entity> = store.panes.keys().copied().collect();

    for entity in entities {
        let Ok(rect) = rects.get(entity) else {
            continue;
        };
        let (content_w, content_h) = content_size(rect.size);

        let Some(pane) = store.panes.get_mut(&entity) else {
            continue;
        };
        let page_h = (content_h - TOOLBAR_H).max(1.0);
        let px = physical_size(content_w, page_h, pane.scale_factor);
        if px == pane.px {
            continue;
        }

        pane.px = px;
        pane.host.send(Cmd::Resize {
            w: content_w,
            h: page_h,
        });
        store.mark_busy(now);
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

fn webview_on_hover(
    _main_thread: NonSendMarker,
    time: Res<Time>,
    mut store: NonSendMut<WebviewStore>,
    mut events: MessageReader<PaneContentHovered>,
) {
    let now = time.elapsed_secs();
    for ev in events.read() {
        store.mark_busy(now);
        let Some(local) = finite(ev.local_pt) else {
            continue;
        };
        let Some(pane) = store.panes.get_mut(&ev.pane) else {
            continue;
        };
        if local.y < TOOLBAR_H {
            continue;
        }
        pane.host.send(Cmd::Mouse {
            x: local.x,
            y: local.y - TOOLBAR_H,
            kind: "move",
        });
    }
}

fn webview_on_press(
    _main_thread: NonSendMarker,
    time: Res<Time>,
    mut store: NonSendMut<WebviewStore>,
    mut events: MessageReader<PaneContentPressed>,
) {
    let now = time.elapsed_secs();
    for ev in events.read() {
        store.mark_busy(now);
        let Some(local) = finite(ev.local_pt) else {
            continue;
        };
        let Some(pane) = store.panes.get_mut(&ev.pane) else {
            continue;
        };

        // Toolbar strip: buttons and the URL bar, not the page.
        if local.y < TOOLBAR_H {
            if (BTN_BACK.0..BTN_BACK.1).contains(&local.x) {
                pane.host.send(Cmd::Back);
            } else if (BTN_FWD.0..BTN_FWD.1).contains(&local.x) {
                pane.host.send(Cmd::Forward);
            } else if (BTN_RELOAD.0..BTN_RELOAD.1).contains(&local.x) {
                pane.host.send(Cmd::Reload);
            } else if local.x >= URL_X {
                // Click the URL bar to edit it; Enter navigates, Esc cancels.
                pane.editing = Some(pane.url.clone());
            }
            continue;
        }
        pane.editing = None;

        pane.host.send(Cmd::Mouse {
            x: local.x,
            y: local.y - TOOLBAR_H,
            kind: "down",
        });
    }
}

fn webview_on_release(
    _main_thread: NonSendMarker,
    time: Res<Time>,
    mut store: NonSendMut<WebviewStore>,
    mut events: MessageReader<PaneContentReleased>,
) {
    let now = time.elapsed_secs();
    for ev in events.read() {
        store.mark_busy(now);
        let Some(local) = finite(ev.local_pt) else {
            continue;
        };
        let Some(pane) = store.panes.get_mut(&ev.pane) else {
            continue;
        };
        if local.y < TOOLBAR_H {
            continue;
        }
        pane.host.send(Cmd::Mouse {
            x: local.x,
            y: local.y - TOOLBAR_H,
            kind: "up",
        });
    }
}

fn webview_on_drag(
    _main_thread: NonSendMarker,
    time: Res<Time>,
    mut store: NonSendMut<WebviewStore>,
    mut events: MessageReader<PaneContentDragged>,
) {
    let now = time.elapsed_secs();
    for ev in events.read() {
        store.mark_busy(now);
        let Some(local) = finite(ev.local_pt) else {
            continue;
        };
        let Some(pane) = store.panes.get_mut(&ev.pane) else {
            continue;
        };
        if local.y < TOOLBAR_H {
            continue;
        }
        pane.host.send(Cmd::Mouse {
            x: local.x,
            y: local.y - TOOLBAR_H,
            kind: "move",
        });
    }
}

/// Wheel over a webview pane scrolls the page. jim's canvas wheel handler
/// targets terminal panes, so the two don't fight; Cmd+scroll stays reserved
/// for canvas zoom.
fn webview_on_wheel(
    _main_thread: NonSendMarker,
    time: Res<Time>,
    mut wheel: MessageReader<MouseWheel>,
    keys: Res<ButtonInput<KeyCode>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    viewport: Res<PaneViewport>,
    panes_q: Query<(Entity, &PaneRect, Option<&Visibility>, &PaneKindMarker), With<PaneTag>>,
    mut store: NonSendMut<WebviewStore>,
) {
    if keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight) {
        wheel.clear();
        return;
    }

    let mut dx = 0.0f32;
    let mut dy = 0.0f32;
    for ev in wheel.read() {
        let (ex, ey) = match ev.unit {
            MouseScrollUnit::Line => (ev.x * SCROLL_LINE_PX, ev.y * SCROLL_LINE_PX),
            MouseScrollUnit::Pixel => (ev.x * SCROLL_PIXEL_GAIN, ev.y * SCROLL_PIXEL_GAIN),
        };
        dx += ex;
        dy += ey;
    }
    if (dx == 0.0 && dy == 0.0) || store.panes.is_empty() {
        return;
    }

    let Ok(window) = windows.single() else {
        return;
    };
    let Some(pt) = window.cursor_position() else {
        return;
    };
    let canvas_pt = viewport.window_to_canvas(pt);

    let rects: Vec<(Entity, PaneRect)> = panes_q
        .iter()
        .filter(|(_, _, vis, kind)| kind.0 == PANE_KIND && !matches!(vis, Some(Visibility::Hidden)))
        .map(|(e, r, _, _)| (e, *r))
        .collect();
    let Some(target) = topmost_pane_at(canvas_pt, &rects) else {
        return;
    };
    let Some(rect) = rects.iter().find(|(e, _)| *e == target).map(|(_, r)| *r) else {
        return;
    };
    let now = time.elapsed_secs();
    let Some(pane) = store.panes.get_mut(&target) else {
        return;
    };

    let Some(local) = finite(pt_to_content_local(canvas_pt, &rect)) else {
        return;
    };
    // Accumulate, then send whole units and keep the remainder.
    pane.scroll_carry.x += dx;
    pane.scroll_carry.y += dy;
    let send_x = pane.scroll_carry.x.trunc();
    let send_y = pane.scroll_carry.y.trunc();
    pane.scroll_carry.x -= send_x;
    pane.scroll_carry.y -= send_y;
    if send_x == 0.0 && send_y == 0.0 {
        return;
    }
    if local.y < TOOLBAR_H {
        return;
    }
    pane.host.send(Cmd::Wheel {
        x: local.x,
        y: local.y - TOOLBAR_H,
        dx: send_x,
        dy: send_y,
    });
    store.mark_busy(now);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Tell CEF which pane (if any) currently has focus.
///
/// A windowless browser never receives focus on its own. Without this a click
/// lands on a text field but the field never takes a caret and typed keys go
/// nowhere — the page behaves as if it is in a background window.
fn webview_track_focus(
    _main_thread: NonSendMarker,
    focused: Res<FocusedPane>,
    mut store: NonSendMut<WebviewStore>,
    mut last: Local<Option<Entity>>,
) {
    if *last == focused.0 {
        return;
    }
    if let Some(prev) = *last {
        if let Some(pane) = store.panes.get_mut(&prev) {
            pane.host.send(Cmd::Focus(false));
            pane.editing = None;
        }
    }
    if let Some(now) = focused.0 {
        if let Some(pane) = store.panes.get_mut(&now) {
            pane.host.send(Cmd::Focus(true));
        }
    }
    *last = focused.0;
}

/// Type into the focused web pane.
///
/// Without this a page renders but is unusable — you cannot fill in a search
/// box or a login form. Gated on `KeyboardOwner` so the command palette and
/// other text modals keep priority.
fn webview_on_keys(
    _main_thread: NonSendMarker,
    time: Res<Time>,
    focused: Res<FocusedPane>,
    owner: Res<KeyboardOwner>,
    mut events: MessageReader<KeyboardInput>,
    mut store: NonSendMut<WebviewStore>,
) {
    let Some(pane_entity) = focused.0 else {
        events.clear();
        return;
    };
    if !owner.allows_pane(pane_entity) || !store.panes.contains_key(&pane_entity) {
        return;
    }
    let now = time.elapsed_secs();

    for ev in events.read() {
        let modifiers = 0u32; // TODO: forward shift/ctrl/alt/cmd chords
        let code = windows_key_code(&ev.logical_key, &ev.key_code);

        let Some(pane) = store.panes.get_mut(&pane_entity) else {
            continue;
        };

        // URL bar has focus: keys edit the address instead of the page.
        if pane.editing.is_some() {
            if ev.state != ButtonState::Pressed {
                continue;
            }
            match &ev.logical_key {
                Key::Enter => {
                    if let Some(text) = pane.editing.take() {
                        if let Some(url) = normalize_url(&text) {
                            pane.url = url.clone();
                            pane.host.send(Cmd::Url(url));
                        }
                    }
                }
                Key::Escape => pane.editing = None,
                Key::Backspace => {
                    if let Some(buf) = pane.editing.as_mut() {
                        buf.pop();
                    }
                }
                Key::Space => {
                    if let Some(buf) = pane.editing.as_mut() {
                        buf.push(' ');
                    }
                }
                Key::Character(s) => {
                    if let Some(buf) = pane.editing.as_mut() {
                        buf.push_str(s);
                    }
                }
                _ => {}
            }
            store.mark_busy(now);
            continue;
        }

        match ev.state {
            ButtonState::Pressed => {
                pane.host.send(Cmd::Key {
                    kind: "down",
                    code,
                    text: None,
                    modifiers,
                });
                // Printable characters additionally need a CHAR event; that is
                // what actually inserts text into the page.
                if let Key::Character(s) = &ev.logical_key {
                    pane.host.send(Cmd::Key {
                        kind: "char",
                        code,
                        text: Some(s.to_string()),
                        modifiers,
                    });
                } else if matches!(ev.logical_key, Key::Space) {
                    pane.host.send(Cmd::Key {
                        kind: "char",
                        code,
                        text: Some(" ".into()),
                        modifiers,
                    });
                } else {
                    // Backspace/Tab/Enter need a CHAR event as well as the
                    // raw keydown: CEF's editing code acts on the character,
                    // so a RAWKEYDOWN alone does nothing in a text field.
                    let ctrl_char = match ev.logical_key {
                        Key::Enter => Some("\r"),
                        Key::Backspace => Some("\u{8}"),
                        Key::Tab => Some("\t"),
                        _ => None,
                    };
                    if let Some(c) = ctrl_char {
                        pane.host.send(Cmd::Key {
                            kind: "char",
                            code,
                            text: Some(c.into()),
                            modifiers,
                        });
                    }
                }
            }
            ButtonState::Released => pane.host.send(Cmd::Key {
                kind: "up",
                code,
                text: None,
                modifiers,
            }),
        }
        store.mark_busy(now);
    }
}

/// Bevy key -> Windows virtual-key code, which is what CEF expects even on
/// macOS. Only the keys a page actually reacts to need to be exact.
fn windows_key_code(logical: &Key, code: &KeyCode) -> i32 {
    match logical {
        Key::Enter => 0x0D,
        Key::Backspace => 0x08,
        Key::Tab => 0x09,
        Key::Escape => 0x1B,
        Key::Space => 0x20,
        Key::Delete => 0x2E,
        Key::ArrowLeft => 0x25,
        Key::ArrowUp => 0x26,
        Key::ArrowRight => 0x27,
        Key::ArrowDown => 0x28,
        Key::Home => 0x24,
        Key::End => 0x23,
        Key::PageUp => 0x21,
        Key::PageDown => 0x22,
        Key::Character(s) => s
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase() as i32)
            .unwrap_or(0),
        _ => match code {
            KeyCode::KeyA => 0x41,
            KeyCode::KeyZ => 0x5A,
            _ => 0,
        },
    }
}

/// Pull a URL out of a pane config, wherever it was put.
///
/// `url` is canonical; `command` is what `jimctl widget --kind webview -- <url>`
/// produces; `params.url` matches how script widgets are configured.
fn url_from_config(config: &Value) -> Option<String> {
    let candidates = [
        config.get("url"),
        config.get("command"),
        config.get("params").and_then(|p| p.get("url")),
    ];
    candidates
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .find_map(normalize_url)
}

/// `example.com` -> `https://example.com`, `localhost:3000` -> `http://…`.
/// Returns None for things that clearly are not URLs, so a stray `command`
/// (e.g. a shell line) does not get loaded as a page.
pub fn normalize_url(raw: &str) -> Option<String> {
    let q = raw.trim();
    if q.is_empty() || q.contains(char::is_whitespace) {
        return None;
    }
    if q.starts_with("http://") || q.starts_with("https://") || q.starts_with("file://") {
        return Some(q.to_string());
    }
    let host = q.split('/').next().unwrap_or(q);
    if host == "localhost" || host.starts_with("localhost:") {
        return Some(format!("http://{q}"));
    }
    let (before, after) = host.split_once('.')?;
    let tld_like = after.len() >= 2 && after.chars().all(|c| c.is_ascii_alphabetic() || c == '.');
    (!before.is_empty() && tld_like).then(|| format!("https://{q}"))
}

/// jim signals "pointer left the pane" with a non-finite sentinel
/// coordinate. `serde_json` writes those as `null`, which the host then
/// rejects, so drop them here rather than putting junk on the wire.
fn finite(p: Vec2) -> Option<Vec2> {
    (p.x.is_finite() && p.y.is_finite()).then_some(p)
}

/// Logical content area inside a pane's chrome.
fn content_size(pane_size: Vec2) -> (f32, f32) {
    (
        (pane_size.x - 2.0 * MARGIN).max(1.0),
        (pane_size.y - TITLE_H - 2.0 * MARGIN).max(1.0),
    )
}

/// CEF renders at logical x scale_factor and the sprite presents it back at
/// logical size, so pages are crisp on retina instead of upscaled.
fn physical_size(content_w: f32, content_h: f32, scale_factor: f32) -> (u32, u32) {
    (
        (content_w * scale_factor).round().max(1.0) as u32,
        (content_h * scale_factor).round().max(1.0) as u32,
    )
}

fn primary_scale_factor(world: &mut World) -> f32 {
    world
        .query_filtered::<&Window, With<PrimaryWindow>>()
        .iter(world)
        .next()
        .map(|w| w.resolution.scale_factor())
        .unwrap_or(1.0)
        .max(1.0)
}

fn blank_image(width: u32, height: u32) -> Image {
    let mut img = Image::new_fill(
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        bevy::render::render_resource::TextureDimension::D2,
        &[0, 0, 0, 255],
        // CEF paints BGRA; matching the format here avoids swizzling every
        // frame on the CPU.
        bevy::render::render_resource::TextureFormat::Bgra8UnormSrgb,
        bevy::asset::RenderAssetUsages::default(),
    );
    img.asset_usage = bevy::asset::RenderAssetUsages::default();
    img
}
