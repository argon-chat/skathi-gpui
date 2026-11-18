//! Pens on Wayland (`zwp_tablet_v2`): a tool's events, gathered up to its `frame`, become gpui
//! [`PenEvent`]s for the window under it.

use super::*;
use gpui::{PenAction, PenEvent, PenInput, PenSample};
use wayland_protocols::wp::tablet::zv2::client::{
    zwp_tablet_manager_v2, zwp_tablet_pad_group_v2, zwp_tablet_pad_ring_v2,
    zwp_tablet_pad_strip_v2, zwp_tablet_pad_v2, zwp_tablet_seat_v2, zwp_tablet_tool_v2,
    zwp_tablet_v2,
};

/// linux/input-event-codes.h: the barrel buttons.
const BTN_STYLUS: u32 = 0x14b;
const BTN_STYLUS2: u32 = 0x14c;
const BTN_STYLUS3: u32 = 0x149;

/// What one tool frame reported.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Frame {
    pub(super) entered: bool,
    pub(super) moved: bool,
    pub(super) down: bool,
    pub(super) up: bool,
    pub(super) left: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Step {
    Down(MouseButton),
    Move(Option<MouseButton>),
    Up(MouseButton),
    Exited,
}

/// The gpui events a frame makes, in order, given the button the tip holds; returns the button
/// held after it.
pub(super) fn steps(
    frame: Frame,
    held: Option<MouseButton>,
    barrel: bool,
) -> (SmallVec<[Step; 4]>, Option<MouseButton>) {
    let mut out = SmallVec::new();
    let mut held = held;
    if frame.entered || frame.moved {
        out.push(Step::Move(held));
    }
    if frame.up {
        if let Some(button) = held.take() {
            out.push(Step::Up(button));
        }
    }
    if frame.down && held.is_none() {
        let button = if barrel {
            MouseButton::Right
        } else {
            MouseButton::Left
        };
        held = Some(button);
        out.push(Step::Down(button));
    }
    if frame.left {
        if let Some(button) = held.take() {
            out.push(Step::Up(button));
        }
        out.push(Step::Exited);
    }
    (out, held)
}

#[derive(Default)]
pub(super) struct Tool {
    eraser: bool,
    window: Option<WaylandWindowStatePtr>,
    cursor: Option<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1>,
    position: Point<Pixels>,
    pressure: f32,
    tilt: (f32, f32),
    twist: f32,
    slider: f32,
    side_buttons: u8,
    held: Option<MouseButton>,
    frame: Frame,
}

impl Tool {
    fn sample(&self, contact: bool) -> PenSample {
        PenSample {
            position: self.position,
            pressure: if contact { self.pressure } else { 0.0 },
            tangential_pressure: self.slider,
            tilt_x: self.tilt.0.clamp(-90.0, 90.0),
            tilt_y: self.tilt.1.clamp(-90.0, 90.0),
            twist: self.twist.rem_euclid(360.0),
            width: 0.0,
            height: 0.0,
        }
    }
}

pub(super) fn tablet_seat(
    globals: &Globals,
    seat: &wl_seat::WlSeat,
) -> Option<zwp_tablet_seat_v2::ZwpTabletSeatV2> {
    let manager: zwp_tablet_manager_v2::ZwpTabletManagerV2 = globals.tablet_manager.clone()?;
    Some(manager.get_tablet_seat(seat, &globals.qh, ()))
}

delegate_noop!(WaylandClientStatePtr: ignore zwp_tablet_manager_v2::ZwpTabletManagerV2);
delegate_noop!(WaylandClientStatePtr: ignore zwp_tablet_v2::ZwpTabletV2);
delegate_noop!(WaylandClientStatePtr: ignore zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2);
delegate_noop!(WaylandClientStatePtr: ignore zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2);

impl Dispatch<zwp_tablet_seat_v2::ZwpTabletSeatV2, ()> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        _: &zwp_tablet_seat_v2::ZwpTabletSeatV2,
        event: zwp_tablet_seat_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_tablet_seat_v2::Event::ToolAdded { id } = event {
            let client = this.get_client();
            client
                .borrow_mut()
                .tablet_tools
                .insert(id.id(), Tool::default());
        }
    }

    event_created_child!(WaylandClientStatePtr, zwp_tablet_seat_v2::ZwpTabletSeatV2, [
        zwp_tablet_seat_v2::EVT_TABLET_ADDED_OPCODE => (zwp_tablet_v2::ZwpTabletV2, ()),
        zwp_tablet_seat_v2::EVT_TOOL_ADDED_OPCODE => (zwp_tablet_tool_v2::ZwpTabletToolV2, ()),
        zwp_tablet_seat_v2::EVT_PAD_ADDED_OPCODE => (zwp_tablet_pad_v2::ZwpTabletPadV2, ()),
    ]);
}

impl Dispatch<zwp_tablet_pad_v2::ZwpTabletPadV2, ()> for WaylandClientStatePtr {
    fn event(
        _: &mut Self,
        _: &zwp_tablet_pad_v2::ZwpTabletPadV2,
        _: zwp_tablet_pad_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    event_created_child!(WaylandClientStatePtr, zwp_tablet_pad_v2::ZwpTabletPadV2, [
        zwp_tablet_pad_v2::EVT_GROUP_OPCODE => (zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, ()),
    ]);
}

impl Dispatch<zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, ()> for WaylandClientStatePtr {
    fn event(
        _: &mut Self,
        _: &zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2,
        _: zwp_tablet_pad_group_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    event_created_child!(WaylandClientStatePtr, zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, [
        zwp_tablet_pad_group_v2::EVT_RING_OPCODE => (zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2, ()),
        zwp_tablet_pad_group_v2::EVT_STRIP_OPCODE => (zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2, ()),
    ]);
}

impl Dispatch<zwp_tablet_tool_v2::ZwpTabletToolV2, ()> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        proxy: &zwp_tablet_tool_v2::ZwpTabletToolV2,
        event: zwp_tablet_tool_v2::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use zwp_tablet_tool_v2::Event;
        let client = this.get_client();
        let mut state = client.borrow_mut();
        let id = proxy.id();
        if let Event::Removed = event {
            if let Some(tool) = state.tablet_tools.remove(&id) {
                if let Some(cursor) = tool.cursor {
                    cursor.destroy();
                }
            }
            proxy.destroy();
            return;
        }
        if let Event::ProximityIn {
            serial, surface, ..
        } = &event
        {
            let window = get_window(&mut state, &surface.id());
            let shape = to_shape(state.cursor_style.unwrap_or(CursorStyle::Arrow));
            let manager = state.globals.cursor_shape_manager.clone();
            if let Some(tool) = state.tablet_tools.get_mut(&id) {
                tool.window = window;
                tool.frame.entered = true;
                if let Some(manager) = manager {
                    tool.cursor
                        .get_or_insert_with(|| manager.get_tablet_tool_v2(proxy, qh, ()))
                        .set_shape(*serial, shape);
                }
            }
            return;
        }
        if let Event::Frame { .. } = event {
            let modifiers = state.modifiers;
            let Some(tool) = state.tablet_tools.get_mut(&id) else {
                return;
            };
            let frame = std::mem::take(&mut tool.frame);
            let barrel = tool.side_buttons != 0;
            let (steps, held) = steps(frame, tool.held, barrel);
            tool.held = held;
            let Some(window) = tool.window.clone() else {
                return;
            };
            if frame.left {
                tool.window = None;
            }
            let position = tool.position;
            let eraser = tool.eraser;
            let (hovering, touching) = (tool.sample(false), tool.sample(true));
            let mut inputs = Vec::with_capacity(steps.len());
            for step in steps {
                let contact = matches!(step, Step::Down(_) | Step::Move(Some(_)));
                let sample = if contact { touching } else { hovering };
                let action = match step {
                    Step::Down(button) => {
                        let click = &mut state.click;
                        if click.last_click.elapsed() < DOUBLE_CLICK_INTERVAL
                            && click.last_mouse_button == Some(button)
                            && is_within_click_distance(click.last_location, position)
                        {
                            click.current_count += 1;
                        } else {
                            click.current_count = 1;
                        }
                        click.last_click = Instant::now();
                        click.last_mouse_button = Some(button);
                        click.last_location = position;
                        PenAction::Down(MouseDownEvent {
                            button,
                            position,
                            modifiers,
                            click_count: click.current_count,
                            first_mouse: false,
                        })
                    }
                    Step::Up(button) => PenAction::Up(MouseUpEvent {
                        button,
                        position,
                        modifiers,
                        click_count: state.click.current_count,
                    }),
                    Step::Move(pressed_button) => PenAction::Move(MouseMoveEvent {
                        position,
                        pressed_button,
                        modifiers,
                    }),
                    Step::Exited => PenAction::Exited(MouseExitEvent {
                        position,
                        pressed_button: None,
                        modifiers,
                    }),
                };
                inputs.push(PlatformInput::Pen(PenEvent {
                    action,
                    pen: PenInput {
                        pointer_id: id.protocol_id(),
                        eraser,
                        barrel,
                        in_contact: contact,
                        sample,
                        history: Vec::new(),
                    },
                }));
            }
            drop(state);
            for input in inputs {
                window.handle_input(input);
            }
            return;
        }
        let Some(tool) = state.tablet_tools.get_mut(&id) else {
            return;
        };
        match event {
            Event::Type {
                tool_type: WEnum::Value(kind),
            } => tool.eraser = kind == zwp_tablet_tool_v2::Type::Eraser,
            Event::ProximityOut => tool.frame.left = true,
            Event::Down { .. } => tool.frame.down = true,
            Event::Up => tool.frame.up = true,
            Event::Motion { x, y } => {
                tool.position = point(px(x as f32), px(y as f32));
                tool.frame.moved = true;
            }
            Event::Pressure { pressure } => {
                tool.pressure = pressure as f32 / 65535.0;
                tool.frame.moved = true;
            }
            Event::Tilt { tilt_x, tilt_y } => {
                tool.tilt = (tilt_x as f32, tilt_y as f32);
                tool.frame.moved = true;
            }
            Event::Rotation { degrees } => {
                tool.twist = degrees as f32;
                tool.frame.moved = true;
            }
            Event::Slider { position } => {
                tool.slider = (position as f32 / 65535.0).clamp(-1.0, 1.0);
                tool.frame.moved = true;
            }
            Event::Button {
                button,
                state: WEnum::Value(pressed),
                ..
            } => {
                let bit = match button {
                    BTN_STYLUS => 1,
                    BTN_STYLUS2 => 2,
                    BTN_STYLUS3 => 4,
                    _ => 0,
                };
                if pressed == zwp_tablet_tool_v2::ButtonState::Pressed {
                    tool.side_buttons |= bit;
                } else {
                    tool.side_buttons &= !bit;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_become_a_hover_a_press_its_moves_and_a_release() {
        use MouseButton::*;
        use Step::*;
        let f = |entered, moved, down, up, left| Frame {
            entered,
            moved,
            down,
            up,
            left,
        };
        let run = |frame, held, barrel| {
            let (steps, held) = steps(frame, held, barrel);
            (steps.to_vec(), held)
        };
        assert_eq!(
            run(f(true, true, false, false, false), None, false),
            (vec![Move(None)], None)
        );
        assert_eq!(
            run(f(false, true, true, false, false), None, false),
            (vec![Move(None), Down(Left)], Some(Left))
        );
        assert_eq!(
            run(f(false, false, true, false, false), None, true),
            (vec![Down(Right)], Some(Right)),
            "the barrel held at contact"
        );
        assert_eq!(
            run(f(false, true, false, false, false), Some(Left), false),
            (vec![Move(Some(Left))], Some(Left))
        );
        assert_eq!(
            run(f(false, false, false, true, false), Some(Left), false),
            (vec![Up(Left)], None)
        );
        assert_eq!(
            run(f(false, false, false, false, true), Some(Left), false),
            (vec![Up(Left), Exited], None),
            "leaving range while touching"
        );
    }
}
