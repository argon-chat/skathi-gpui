//! XDND source. The X server's implicit button grab keeps the initiating
//! press alive across applications. Select the aware window under the pointer
//! and wait for XdndFinished before telling the document what happened.
use super::*;
use crate::linux::drag_source::{MARKER, Offer};
use gpui::{DropOperation, PlatformDragData};
use std::sync::Arc;
use x11rb::protocol::xproto::{KeyButMask, PropMode, Property, SelectionNotifyEvent};

#[derive(Clone, Copy, PartialEq)]
struct Target {
    window: u32,
    proxy: u32,
    version: u32,
}

pub(super) struct Session {
    source: u32,
    root: u32,
    data: Arc<Offer>,
    formats: Vec<(u32, String)>,
    allowed: u8,
    target: Option<Target>,
    last: Option<(i16, i16, u32)>,
    action: DropOperation,
    dropped: Option<Instant>,
    done: Box<dyn FnOnce(DropOperation)>,
}

impl Session {
    pub(super) fn source_window(&self) -> u32 {
        self.source
    }
}
pub(super) struct Increment {
    bytes: Vec<u8>,
    at: usize,
    target: u32,
    chunk: usize,
    expires: Instant,
}

fn property(connection: &XCBConnection, window: u32, atom: u32) -> Option<u32> {
    connection
        .get_property(false, window, atom, AtomEnum::ANY, 0, 1)
        .ok()?
        .reply()
        .ok()?
        .value32()?
        .next()
}
fn send(connection: &XCBConnection, target: Target, ty: u32, data: [u32; 5]) {
    let message = ClientMessageEvent::new(32, target.window, ty, ClientMessageData::from(data));
    let _ = connection.send_event(false, target.proxy, EventMask::NO_EVENT, message);
}
fn target_under(connection: &XCBConnection, atoms: &XcbAtoms, first: u32) -> Option<Target> {
    let mut next = first;
    let mut candidate = None;
    for _ in 0..64 {
        if next == 0 {
            break;
        }
        if let Some(version) = property(connection, next, atoms.XdndAware).filter(|v| *v >= 3) {
            let proxy = property(connection, next, atoms.XdndProxy)
                .filter(|p| *p != 0 && property(connection, *p, atoms.XdndProxy) == Some(*p))
                .unwrap_or(next);
            candidate = Some(Target {
                window: next,
                proxy,
                version: version.min(5),
            });
        }
        next = connection.query_pointer(next).ok()?.reply().ok()?.child;
    }
    candidate
}
fn requested(allowed: u8, mask: KeyButMask) -> DropOperation {
    let preferred = if mask.contains(KeyButMask::MOD1)
        || mask.contains(KeyButMask::CONTROL | KeyButMask::SHIFT)
    {
        DropOperation::Link
    } else if mask.contains(KeyButMask::SHIFT) {
        DropOperation::Move
    } else {
        DropOperation::Copy
    };
    [
        preferred,
        DropOperation::Copy,
        DropOperation::Move,
        DropOperation::Link,
    ]
    .into_iter()
    .find(|op| allowed & (*op as u8) != 0)
    .unwrap_or_default()
}
fn permitted(allowed: u8, action: u32, atoms: &XcbAtoms) -> DropOperation {
    [
        DropOperation::Copy,
        DropOperation::Move,
        DropOperation::Link,
    ]
    .into_iter()
    .find(|op| allowed & (*op as u8) != 0 && xdnd_action(*op, atoms) == action)
    .unwrap_or_default()
}

impl X11Client {
    pub(crate) fn can_start_document_drag(&self, source: u32) -> bool {
        let state = self.0.borrow();
        state.document_drag.is_none()
            && state
                .xcb_connection
                .query_pointer(source)
                .ok()
                .and_then(|q| q.reply().ok())
                .is_some_and(|r| r.same_screen && r.mask.contains(KeyButMask::BUTTON1))
    }
    pub(crate) fn start_document_drag(
        &self,
        source: u32,
        data: PlatformDragData,
        done: Box<dyn FnOnce(DropOperation)>,
    ) -> bool {
        if !self.can_start_document_drag(source) {
            return false;
        }
        let mut state = self.0.borrow_mut();
        let conn = state.xcb_connection.clone();
        let atoms = &state.atoms;
        let offer = Offer::document(&data);
        let mut formats: Vec<_> = offer
            .types()
            .into_iter()
            .filter_map(|mime| {
                let atom = conn
                    .intern_atom(false, mime.as_bytes())
                    .ok()?
                    .reply()
                    .ok()?
                    .atom;
                Some((atom, mime.to_string()))
            })
            .collect();
        if formats.is_empty() {
            return false;
        }
        // Ordinary applications should pick a public type first. Our receiver
        // authenticates the process-local store using the private type alone.
        formats.sort_by_key(|(_, mime)| match mime.as_str() {
            "text/uri-list" => 0,
            "text/plain;charset=utf-8" => 1,
            MARKER => 3,
            _ => 2,
        });
        let types: Vec<_> = formats.iter().map(|(id, _)| *id).collect();
        let actions: Vec<_> = [
            DropOperation::Copy,
            DropOperation::Move,
            DropOperation::Link,
        ]
        .into_iter()
        .filter(|op| data.allowed & (*op as u8) != 0)
        .map(|op| xdnd_action(op, atoms))
        .collect();
        if conn
            .change_property32(
                PropMode::REPLACE,
                source,
                atoms.XdndTypeList,
                AtomEnum::ATOM,
                &types,
            )
            .is_err()
            || conn
                .change_property32(
                    PropMode::REPLACE,
                    source,
                    atoms.XdndActionList,
                    AtomEnum::ATOM,
                    &actions,
                )
                .is_err()
            || conn
                .set_selection_owner(source, atoms.XdndSelection, x11rb::CURRENT_TIME)
                .is_err()
            || !conn
                .get_selection_owner(atoms.XdndSelection)
                .ok()
                .and_then(|r| r.reply().ok())
                .is_some_and(|r| r.owner == source)
        {
            return false;
        }
        let root = conn.setup().roots[state.x_root_index].root;
        state.document_drag = Some(Session {
            source,
            root,
            data: offer,
            formats,
            allowed: data.allowed,
            target: None,
            last: None,
            action: DropOperation::None,
            dropped: None,
            done,
        });
        let common = state.common.borrow();
        let fg = common.foreground_executor.clone();
        let bg = common.background_executor.clone();
        drop(common);
        drop(state);
        let weak = Rc::downgrade(&self.0);
        fg.spawn(async move {
            loop {
                let Some(state) = weak.upgrade() else { break };
                if !X11Client(state).document_tick(source) {
                    break;
                }
                bg.timer(Duration::from_millis(16)).await;
            }
        })
        .detach();
        true
    }
    fn document_tick(&self, source: u32) -> bool {
        let state = self.0.borrow();
        let conn = state.xcb_connection.clone();
        let Some(session) = state.document_drag.as_ref().filter(|s| s.source == source) else {
            return false;
        };
        if let Some(at) = session.dropped {
            if at.elapsed() < Duration::from_secs(30) {
                return true;
            }
            drop(state);
            self.complete_document(DropOperation::None);
            return false;
        }
        let reply = conn
            .query_pointer(session.root)
            .ok()
            .and_then(|r| r.reply().ok());
        drop(state);
        let Some(pos) = reply else {
            self.complete_document(DropOperation::None);
            return false;
        };
        let mut state = self.0.borrow_mut();
        let X11ClientState {
            atoms,
            document_drag,
            ..
        } = &mut *state;
        let Some(session) = document_drag.as_mut() else {
            return false;
        };
        let target = target_under(&conn, atoms, pos.child);
        if session.target != target {
            if let Some(old) = session.target {
                send(&conn, old, atoms.XdndLeave, [source, 0, 0, 0, 0]);
            }
            session.target = target;
            session.action = DropOperation::None;
            session.last = None;
            if let Some(t) = target {
                let mut args = [
                    source,
                    t.version << 24 | u32::from(session.formats.len() > 3),
                    0,
                    0,
                    0,
                ];
                for (slot, (ty, _)) in args[2..].iter_mut().zip(&session.formats) {
                    *slot = *ty;
                }
                send(&conn, t, atoms.XdndEnter, args);
            }
        }
        if !pos.mask.contains(KeyButMask::BUTTON1) {
            if let Some(t) = target.filter(|_| session.action != DropOperation::None) {
                send(
                    &conn,
                    t,
                    atoms.XdndDrop,
                    [source, 0, x11rb::CURRENT_TIME, 0, 0],
                );
                session.dropped = Some(Instant::now());
                let _ = conn.flush();
                return true;
            }
            drop(state);
            self.complete_document(DropOperation::None);
            return false;
        }
        if let Some(t) = target {
            let op = xdnd_action(requested(session.allowed, pos.mask), atoms);
            let key = (pos.root_x, pos.root_y, op);
            if session.last != Some(key) {
                session.last = Some(key);
                let xy = ((pos.root_x as u16 as u32) << 16) | (pos.root_y as u16 as u32);
                send(
                    &conn,
                    t,
                    atoms.XdndPosition,
                    [source, 0, xy, x11rb::CURRENT_TIME, op],
                );
            }
        }
        let _ = conn.flush();
        true
    }
    fn complete_document(&self, op: DropOperation) {
        let mut state = self.0.borrow_mut();
        let Some(session) = state.document_drag.take() else {
            return;
        };
        let conn = &state.xcb_connection;
        if op == DropOperation::None
            && session.dropped.is_none()
            && let Some(t) = session.target
        {
            send(conn, t, state.atoms.XdndLeave, [session.source, 0, 0, 0, 0]);
        }
        if conn
            .get_selection_owner(state.atoms.XdndSelection)
            .ok()
            .and_then(|r| r.reply().ok())
            .is_some_and(|r| r.owner == session.source)
        {
            let _ = conn.set_selection_owner(0u32, state.atoms.XdndSelection, x11rb::CURRENT_TIME);
        }
        let _ = conn.delete_property(session.source, state.atoms.XdndTypeList);
        let _ = conn.delete_property(session.source, state.atoms.XdndActionList);
        let _ = conn.flush();
        drop(state);
        (session.done)(op);
    }

    pub(super) fn document_event(&self, event: &Event) -> bool {
        let mut state = self.0.borrow_mut();
        let conn = state.xcb_connection.clone();
        state
            .document_increments
            .retain(|_, i| i.expires > Instant::now());
        if let Event::PropertyNotify(e) = event
            && e.state == Property::DELETE
        {
            let key = (e.window, e.atom);
            if let Some(i) = state.document_increments.get_mut(&key) {
                let end = (i.at + i.chunk).min(i.bytes.len());
                let _ = conn.change_property8(
                    PropMode::REPLACE,
                    e.window,
                    e.atom,
                    i.target,
                    &i.bytes[i.at..end],
                );
                if i.at == end {
                    state.document_increments.remove(&key);
                } else {
                    i.at = end;
                    i.expires = Instant::now() + Duration::from_secs(60);
                }
                let _ = conn.flush();
                return true;
            }
        }
        let X11ClientState {
            atoms,
            document_drag,
            xkb,
            ..
        } = &mut *state;
        let Some(session) = document_drag.as_mut() else {
            return false;
        };
        match event {
            Event::ClientMessage(e)
                if e.window == session.source
                    && (e.type_ == atoms.XdndStatus || e.type_ == atoms.XdndFinished) =>
            {
                let [window, flag, third, _, fifth] = e.data.as_data32();
                let Some(t) = session
                    .target
                    .filter(|t| t.window == window || t.proxy == window)
                else {
                    return true;
                };
                if e.type_ == atoms.XdndStatus {
                    session.action = if flag & 1 != 0 {
                        permitted(session.allowed, fifth, atoms)
                    } else {
                        DropOperation::None
                    };
                } else if session.dropped.is_some() {
                    let op = if t.version < 5 {
                        session.action
                    } else if flag & 1 != 0 {
                        permitted(session.allowed, third, atoms)
                    } else {
                        DropOperation::None
                    };
                    drop(state);
                    self.complete_document(op);
                }
                true
            }
            Event::SelectionClear(e)
                if e.selection == atoms.XdndSelection && e.owner == session.source =>
            {
                drop(state);
                self.complete_document(DropOperation::None);
                true
            }
            Event::DestroyNotify(e) if e.window == session.source => {
                drop(state);
                self.complete_document(DropOperation::None);
                false
            }
            Event::KeyPress(e)
                if xkb
                    .key_get_one_sym(xkbc::Keycode::new(e.detail.into()))
                    .raw()
                    == 0xff1b =>
            {
                drop(state);
                self.complete_document(DropOperation::None);
                true
            }
            Event::SelectionRequest(e)
                if e.owner == session.source && e.selection == atoms.XdndSelection =>
            {
                let e = *e;
                let property = if e.property == 0 {
                    e.target
                } else {
                    e.property
                };
                if e.target == atoms.TARGETS {
                    let mut types: Vec<_> = session.formats.iter().map(|f| f.0).collect();
                    types.push(atoms.TARGETS);
                    let ok = conn
                        .change_property32(
                            PropMode::REPLACE,
                            e.requestor,
                            property,
                            AtomEnum::ATOM,
                            &types,
                        )
                        .is_ok();
                    selection_reply(&conn, &e, if ok { property } else { 0 });
                } else if let Some((_, mime)) = session.formats.iter().find(|(a, _)| *a == e.target)
                {
                    let mime = mime.clone();
                    let offer = session.data.clone();
                    let fg = state.common.borrow().foreground_executor.clone();
                    let bg = state.common.borrow().background_executor.clone();
                    let weak = Rc::downgrade(&self.0);
                    fg.spawn(async move {
                        let bytes = bg.spawn(async move { offer.bytes(&mime) }).await;
                        let Some(state) = weak.upgrade() else { return };
                        let mut state = state.borrow_mut();
                        let max = conn
                            .maximum_request_bytes()
                            .saturating_sub(128)
                            .min(256 * 1024);
                        let ok = match bytes {
                            Ok(bytes) if bytes.len() <= max => conn
                                .change_property8(
                                    PropMode::REPLACE,
                                    e.requestor,
                                    property,
                                    e.target,
                                    &bytes,
                                )
                                .is_ok(),
                            Ok(bytes) if max > 0 => {
                                let mask = conn
                                    .get_window_attributes(e.requestor)
                                    .ok()
                                    .and_then(|r| r.reply().ok())
                                    .map(|r| r.your_event_mask | EventMask::PROPERTY_CHANGE);
                                let ok = mask.is_some_and(|mask| {
                                    conn.change_window_attributes(
                                        e.requestor,
                                        &ChangeWindowAttributesAux::new().event_mask(mask),
                                    )
                                    .is_ok()
                                }) && conn
                                    .change_property32(
                                        PropMode::REPLACE,
                                        e.requestor,
                                        property,
                                        state.atoms.INCR_DND,
                                        &[u32::try_from(bytes.len()).unwrap_or(u32::MAX)],
                                    )
                                    .is_ok();
                                if ok {
                                    state.document_increments.insert(
                                        (e.requestor, property),
                                        Increment {
                                            bytes,
                                            at: 0,
                                            target: e.target,
                                            chunk: max,
                                            expires: Instant::now() + Duration::from_secs(60),
                                        },
                                    );
                                }
                                ok
                            }
                            _ => false,
                        };
                        selection_reply(&conn, &e, if ok { property } else { 0 });
                    })
                    .detach();
                } else {
                    selection_reply(&conn, &e, 0);
                }
                true
            }
            _ => false,
        }
    }
}

fn selection_reply(conn: &XCBConnection, req: &xproto::SelectionRequestEvent, property: u32) {
    let reply = SelectionNotifyEvent {
        response_type: xproto::SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: req.time,
        requestor: req.requestor,
        selection: req.selection,
        target: req.target,
        property,
    };
    let _ = conn.send_event(false, req.requestor, EventMask::NO_EVENT, reply);
    let _ = conn.flush();
}
