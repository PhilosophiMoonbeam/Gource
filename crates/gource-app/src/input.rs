// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Input translation for the native presenter.
//!
//! The state machine is intentionally independent from an egui context.  The
//! caller supplies egui's per-event `consumed` bit and camera manipulation is
//! gated on that bit, so text fields and sliders cannot also pan or zoom the
//! scene.

use gource_core::CameraMode;
use gource_render::{RenderPoint, RenderView};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};

/// Presentation-only camera mode.  `Manual` never enters replay identity and
/// is therefore safe to change while a session is running.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CameraControlMode {
    #[default]
    Overview,
    Track,
    Manual,
}

impl From<CameraMode> for CameraControlMode {
    fn from(value: CameraMode) -> Self {
        match value {
            CameraMode::Overview => Self::Overview,
            CameraMode::Track => Self::Track,
        }
    }
}

/// Mutable presentation camera.  Coordinates are in the simulation's world
/// space and match [`gource_render::RenderView`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraController {
    pub mode: CameraControlMode,
    pub center: [f32; 2],
    pub zoom: f32,
    pub rotation_radians: f32,
    pub viewport: [u32; 2],
}

impl Default for CameraController {
    fn default() -> Self {
        Self {
            mode: CameraControlMode::Overview,
            center: [0.0, 0.0],
            zoom: 1.0,
            rotation_radians: 0.0,
            viewport: [1, 1],
        }
    }
}

impl CameraController {
    pub fn set_viewport(&mut self, width: u32, height: u32) {
        self.viewport = [width, height];
    }

    pub fn set_mode(&mut self, mode: CameraControlMode) {
        self.mode = mode;
    }

    pub fn reset(&mut self) {
        self.mode = CameraControlMode::Overview;
        self.center = [0.0, 0.0];
        self.zoom = 1.0;
        self.rotation_radians = 0.0;
    }

    pub fn pan_pixels(&mut self, delta_x: f64, delta_y: f64) {
        if !delta_x.is_finite() || !delta_y.is_finite() {
            return;
        }
        let width = self.viewport[0].max(1) as f64;
        let height = self.viewport[1].max(1) as f64;
        let aspect = width / height;
        let zoom = if self.zoom.is_finite() && self.zoom > 0.0 {
            f64::from(self.zoom)
        } else {
            1.0
        };
        let scale = 2.0 / zoom;
        self.center[0] -= (delta_x / width * scale * aspect) as f32;
        self.center[1] += (delta_y / height * scale) as f32;
    }

    pub fn zoom_by(&mut self, amount: f64) {
        if !amount.is_finite() {
            return;
        }
        let multiplier = (amount * 0.1).exp().clamp(0.05, 20.0) as f32;
        let zoom = if self.zoom.is_finite() && self.zoom > 0.0 {
            self.zoom
        } else {
            1.0
        };
        self.zoom = (zoom * multiplier).clamp(0.02, 100.0);
    }

    pub fn view(&self) -> RenderView {
        RenderView {
            width: self.viewport[0],
            height: self.viewport[1],
            center: RenderPoint::new(self.center[0], self.center[1]),
            zoom: self.zoom,
            rotation_radians: self.rotation_radians,
        }
    }
}

/// Commands emitted by keyboard, pointer, and UI controls.
#[derive(Clone, Debug, PartialEq)]
pub enum AppCommand {
    TogglePause,
    SetPaused(bool),
    AdjustPlaybackRate(f64),
    SetPlaybackRate(f64),
    SeekTick(u64),
    SeekRepositorySeconds(f64),
    NextEvent,
    SetCameraMode(CameraControlMode),
    ResetCamera,
    Zoom(f32),
    SelectAt { x: f64, y: f64 },
    ClearSelection,
    Quit,
}

/// Selection hit-test input in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelectionPoint {
    pub x: f64,
    pub y: f64,
}

/// Translation state for all native input devices.
#[derive(Clone, Debug, PartialEq)]
pub struct InputState {
    pub camera: CameraController,
    pub paused: bool,
    pub playback_rate: f64,
    pub cursor_position: Option<[f64; 2]>,
    pub selected: Option<SelectionPoint>,
    pub right_dragging: bool,
    pub left_selecting: bool,
    pub focused: bool,
    pub modifiers: ModifiersState,
    last_cursor_position: Option<[f64; 2]>,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            camera: CameraController::default(),
            paused: false,
            playback_rate: 1.0,
            cursor_position: None,
            selected: None,
            right_dragging: false,
            left_selecting: false,
            focused: false,
            modifiers: ModifiersState::empty(),
            last_cursor_position: None,
        }
    }
}
impl InputState {
    pub fn set_viewport(&mut self, width: u32, height: u32) {
        self.camera.set_viewport(width, height);
    }

    pub fn set_camera_mode(&mut self, mode: CameraControlMode) {
        self.camera.set_mode(mode);
    }

    /// Handle a native event after egui has had a chance to consume it.
    ///
    /// `CursorMoved` coordinates are physical pixels, matching the physical
    /// camera viewport.  `consumed == true` is a hard barrier for camera and
    /// transport actions.  We still record cursor/focus state so a later
    /// unconsumed event starts from a correct position.
    pub fn on_window_event(&mut self, event: &WindowEvent, consumed: bool) -> Vec<AppCommand> {
        match event {
            WindowEvent::Focused(focused) => {
                self.focused = *focused;
                if !focused {
                    self.right_dragging = false;
                    self.left_selecting = false;
                    self.cursor_position = None;
                    self.last_cursor_position = None;
                }
                Vec::new()
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
                Vec::new()
            }
            WindowEvent::KeyboardInput { event, .. } => self.on_key_event(event, consumed),
            WindowEvent::CursorMoved { position, .. } => {
                self.on_cursor_moved(position.x, position.y, consumed);
                Vec::new()
            }
            WindowEvent::CursorLeft { .. } | WindowEvent::ScaleFactorChanged { .. } => {
                // Cursor coordinates are physical.  A scale change can alter
                // their physical representation without a corresponding
                // movement event, so never use the previous point as a drag
                // delta after either transition.
                self.cursor_position = None;
                self.last_cursor_position = None;
                Vec::new()
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.on_mouse_input(*state, *button, consumed)
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.on_mouse_wheel(delta, consumed);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Keyboard-only entry point useful for deterministic input tests.
    pub fn on_key(
        &mut self,
        physical_key: PhysicalKey,
        state: ElementState,
        repeat: bool,
        consumed: bool,
    ) -> Vec<AppCommand> {
        self.on_key_code(physical_key, state, repeat, consumed)
    }

    fn on_key_event(&mut self, event: &KeyEvent, consumed: bool) -> Vec<AppCommand> {
        self.on_key_code(event.physical_key, event.state, event.repeat, consumed)
    }

    fn on_key_code(
        &mut self,
        physical_key: PhysicalKey,
        state: ElementState,
        repeat: bool,
        consumed: bool,
    ) -> Vec<AppCommand> {
        if state != ElementState::Pressed || repeat || consumed {
            return Vec::new();
        }
        let key = match physical_key {
            PhysicalKey::Code(code) => code,
            PhysicalKey::Unidentified(_) => return Vec::new(),
        };
        let command = match key {
            KeyCode::Space | KeyCode::KeyP => AppCommand::TogglePause,
            KeyCode::ArrowRight | KeyCode::KeyN => AppCommand::NextEvent,
            KeyCode::ArrowLeft => AppCommand::SeekRepositorySeconds(-1.0),
            KeyCode::Equal => AppCommand::AdjustPlaybackRate(2.0),
            KeyCode::Minus => AppCommand::AdjustPlaybackRate(0.5),
            KeyCode::NumpadAdd => AppCommand::Zoom(1.1),
            KeyCode::NumpadSubtract => AppCommand::Zoom(1.0 / 1.1),
            KeyCode::KeyO => AppCommand::SetCameraMode(CameraControlMode::Overview),
            KeyCode::KeyT => AppCommand::SetCameraMode(CameraControlMode::Track),
            KeyCode::KeyM => AppCommand::SetCameraMode(CameraControlMode::Manual),
            KeyCode::KeyR => AppCommand::ResetCamera,
            KeyCode::Escape => AppCommand::Quit,
            KeyCode::KeyQ => AppCommand::Quit,
            _ => return Vec::new(),
        };
        vec![command]
    }

    fn on_cursor_moved(&mut self, x: f64, y: f64, consumed: bool) {
        if !x.is_finite() || !y.is_finite() {
            self.cursor_position = None;
            self.last_cursor_position = None;
            return;
        }
        let current = [x, y];
        self.cursor_position = Some(current);
        let previous = self.last_cursor_position.replace(current);
        if !consumed
            && self.right_dragging
            && let Some(previous) = previous
        {
            self.camera
                .pan_pixels(current[0] - previous[0], current[1] - previous[1]);
            self.camera.mode = CameraControlMode::Manual;
        }
    }

    fn on_mouse_input(
        &mut self,
        state: ElementState,
        button: MouseButton,
        consumed: bool,
    ) -> Vec<AppCommand> {
        match (button, state) {
            (MouseButton::Right, ElementState::Pressed) if !consumed => {
                self.right_dragging = true;
                self.camera.mode = CameraControlMode::Manual;
                self.last_cursor_position = self.cursor_position;
            }
            (MouseButton::Right, ElementState::Released) => {
                self.right_dragging = false;
                self.last_cursor_position = self.cursor_position;
            }
            (MouseButton::Left, ElementState::Pressed) if !consumed => {
                self.left_selecting = true;
            }
            (MouseButton::Left, ElementState::Released) => {
                let was_selecting = self.left_selecting;
                self.left_selecting = false;
                if was_selecting
                    && !consumed
                    && let Some([x, y]) = self.cursor_position
                {
                    self.selected = Some(SelectionPoint { x, y });
                    return vec![AppCommand::SelectAt { x, y }];
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn on_mouse_wheel(&mut self, delta: &MouseScrollDelta, consumed: bool) {
        if consumed {
            return;
        }
        let amount = match delta {
            MouseScrollDelta::LineDelta(_, y) => f64::from(*y),
            MouseScrollDelta::PixelDelta(position) => position.y / 40.0,
        };
        self.camera.zoom_by(amount);
        self.camera.mode = CameraControlMode::Manual;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumed_keyboard_input_never_emits_transport_commands() {
        let mut input = InputState::default();
        assert!(
            input
                .on_key(
                    PhysicalKey::Code(KeyCode::Space),
                    ElementState::Pressed,
                    false,
                    true
                )
                .is_empty()
        );
        assert!(!input.paused);
    }

    #[test]
    fn main_plus_and_minus_adjust_playback_rate_only() {
        let mut input = InputState::default();
        assert_eq!(
            input.on_key(
                PhysicalKey::Code(KeyCode::Equal),
                ElementState::Pressed,
                false,
                false,
            ),
            vec![AppCommand::AdjustPlaybackRate(2.0)]
        );
        assert_eq!(
            input.on_key(
                PhysicalKey::Code(KeyCode::Minus),
                ElementState::Pressed,
                false,
                false,
            ),
            vec![AppCommand::AdjustPlaybackRate(0.5)]
        );
    }

    #[test]
    fn keypad_plus_and_minus_emit_camera_zoom_commands_only() {
        let mut input = InputState::default();
        assert_eq!(
            input.on_key(
                PhysicalKey::Code(KeyCode::NumpadAdd),
                ElementState::Pressed,
                false,
                false,
            ),
            vec![AppCommand::Zoom(1.1)]
        );
        assert_eq!(
            input.on_key(
                PhysicalKey::Code(KeyCode::NumpadSubtract),
                ElementState::Pressed,
                false,
                false,
            ),
            vec![AppCommand::Zoom(1.0 / 1.1)]
        );
    }

    #[test]
    fn escape_and_q_quit() {
        let mut input = InputState::default();
        for key in [KeyCode::Escape, KeyCode::KeyQ] {
            assert_eq!(
                input.on_key(PhysicalKey::Code(key), ElementState::Pressed, false, false,),
                vec![AppCommand::Quit]
            );
        }
    }

    #[test]
    fn consumed_zoom_and_quit_keys_emit_nothing() {
        let mut input = InputState::default();
        for key in [
            KeyCode::Equal,
            KeyCode::Minus,
            KeyCode::NumpadAdd,
            KeyCode::NumpadSubtract,
            KeyCode::Escape,
        ] {
            assert!(
                input
                    .on_key(PhysicalKey::Code(key), ElementState::Pressed, false, true,)
                    .is_empty()
            );
        }
    }

    #[test]
    fn consumed_pointer_input_never_starts_or_moves_camera() {
        let mut input = InputState::default();
        input.set_viewport(1000, 500);
        let before = input.camera;
        input.on_cursor_moved(10.0, 10.0, true);
        input.on_mouse_input(ElementState::Pressed, MouseButton::Right, true);
        input.on_cursor_moved(100.0, 100.0, false);
        assert!(!input.right_dragging);
        assert_eq!(input.camera, before);
    }

    #[test]
    fn right_drag_pans_and_switches_to_manual_mode() {
        let mut input = InputState::default();
        input.set_viewport(1000, 500);
        input.on_cursor_moved(100.0, 100.0, false);
        input.on_mouse_input(ElementState::Pressed, MouseButton::Right, false);
        input.on_cursor_moved(200.0, 140.0, false);
        assert!(input.right_dragging);
        assert_eq!(input.camera.mode, CameraControlMode::Manual);
        assert_ne!(input.camera.center, [0.0, 0.0]);
    }

    #[test]
    fn consumed_scroll_does_not_change_zoom() {
        let mut input = InputState::default();
        let before = input.camera.zoom;
        input.on_mouse_wheel(&MouseScrollDelta::LineDelta(0.0, 4.0), true);
        assert_eq!(input.camera.zoom, before);
    }

    #[test]
    fn physical_cursor_coordinates_are_emitted_without_scale_conversion() {
        let mut input = InputState::default();
        input.on_cursor_moved(240.0, 120.0, false);
        input.on_mouse_input(ElementState::Pressed, MouseButton::Left, false);
        assert_eq!(
            input.on_mouse_input(ElementState::Released, MouseButton::Left, false),
            vec![AppCommand::SelectAt { x: 240.0, y: 120.0 }]
        );
        assert_eq!(input.selected, Some(SelectionPoint { x: 240.0, y: 120.0 }));
    }

    #[test]
    fn invalid_cursor_coordinates_reset_pointer_tracking() {
        let mut input = InputState::default();
        input.on_cursor_moved(10.0, 20.0, false);
        input.on_mouse_input(ElementState::Pressed, MouseButton::Right, false);
        input.on_cursor_moved(f64::NAN, 20.0, false);
        assert!(input.cursor_position.is_none());
        assert!(!input.camera.center.iter().any(|value| !value.is_finite()));
        input.on_cursor_moved(30.0, 40.0, false);
        assert_eq!(input.camera.center, [0.0, 0.0]);
    }

    #[test]
    fn focus_loss_clears_stale_pointer_coordinates() {
        let mut input = InputState::default();
        input.on_cursor_moved(10.0, 20.0, false);
        input.on_mouse_input(ElementState::Pressed, MouseButton::Right, false);
        input.on_window_event(&WindowEvent::Focused(false), false);
        assert!(!input.right_dragging);
        assert!(input.cursor_position.is_none());
    }
}
