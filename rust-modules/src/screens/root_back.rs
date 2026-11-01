//! **What BACK does at a root** (issue #29), a page of the Settings stack: the television's Home, end
//! the process, or open another app the viewer names.
//!
//! The first two are ordinary choices. *Open another app* takes its id from the television's
//! keyboard: OK on the row raises it, the row's detail shows the draft, OK or the panel closing
//! saves a valid id, and BACK leaves without saving. The value lands through
//! [`PreferenceCmd::RootBack`] and the check mark moves only on a durable receipt.
use std::borrow::Cow;
use std::sync::mpsc::{Receiver, TryRecvError};
use crate::catalog::session::RootBack;
use crate::ui::form::{Form, FormId, FormSection, FormTable, RowKey, RowKind};
use crate::ui::frame::Budget;
use crate::ui::route_screen::RouteLayout;
use crate::ui::screen::{DrawFrame, Enter, FocusSource, FocusTarget, HitSource, RenderStrategy, Screen, ScreenEvent};
use crate::ui::table::Row;
use crate::ui::table_screen::{Header, TableScreen};
use crate::ui::text_buffer::TextBuffer;
use crate::ui::Rect;
use nj_machine::machine::{Canon, Cx, Delivery, Edge, Effects, EntryId, FocusKey, Fx, GroupId,
    Handled, InputEvent, InputKind, InstanceId, Key, LogicalState, Machine, MachineId, TextEdit};
use nj_platform::i18n::msg;
use super::family::{form_activate, form_focus, form_right_target, InnerHost, SettingsPage};
use super::registry::{AppFx, PreferenceCmd, BAND};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowId { Home, Exit, App }

impl FormId for RowId {
    fn key(&self) -> RowKey {
        RowKey(match self { RowId::Home => 0, RowId::Exit => 1, RowId::App => 2 })
    }
}

fn row_of(choice: &RootBack) -> RowId {
    match choice {
        RootBack::PlatformHome => RowId::Home,
        RootBack::Exit => RowId::Exit,
        RootBack::LaunchApp(_) => RowId::App,
    }
}

/// The page's rows from plain inputs. `shown_id` is the text under *Open another app*: the draft
/// while it is being typed, else the saved id, else nothing.
fn root_back_form(choice: &RootBack, shown_id: &str, busy: bool) -> Form<RowId, RowId, SettingsPage> {
    let row = |id: RowId, label: &str| Row::new(label).checked(row_of(choice) == id).dim(busy);
    let app = row(RowId::App, msg::settings_root_back_app());
    let app = if shown_id.is_empty() { app.detail(msg::settings_root_back_app_detail()) } else { app.detail(shown_id).server_detail() };
    let choices = FormSection::new("")
        .item(RowId::Home, RowKind::Choice, RowId::Home, row(RowId::Home, msg::settings_root_back_home()).detail(msg::settings_root_back_home_detail()))
        .item(RowId::Exit, RowKind::Choice, RowId::Exit, row(RowId::Exit, msg::settings_root_back_exit()).detail(msg::settings_root_back_exit_detail()))
        .item(RowId::App, RowKind::Choice, RowId::App, app);
    Form::new().section(choices)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Notice { None, Invalid, SaveFailed }

struct State {
    choice: RootBack,
    /// The focused row's key — an identity, not a position.
    sel: RowKey,
    busy: bool,
    notice: Notice,
    /// The id being typed, while the keyboard is the row's.
    draft: Option<TextBuffer>,
}

impl LogicalState for State {
    fn write(&self, w: &mut Canon) {
        w.u32(self.sel.0).u8(row_of(&self.choice) as u8).bool(self.busy).u8(self.notice as u8);
        if let RootBack::LaunchApp(id) = &self.choice { w.str(id); }
        w.bool(self.draft.is_some());
        if let Some(draft) = &self.draft { w.str(draft.text()).u32(draft.caret() as u32); }
    }
    fn probe(&self, out: &mut String) {
        out.push_str(&format!("root-back {:?} sel={} busy={} notice={:?} editing={}",
            row_of(&self.choice), self.sel.0, self.busy, self.notice, self.draft.is_some()));
    }
}

pub(crate) struct RootBackPage {
    entry: EntryId,
    form: FormTable<RowId, RowId, SettingsPage>,
    state: State,
    save: Option<(RootBack, Receiver<bool>)>,
}

impl RootBackPage {
    pub(crate) fn new(entry: EntryId) -> Self {
        let choice = crate::route::root_back();
        let sel = row_of(&choice).key();
        let mut page = Self {
            entry,
            form: FormTable::new(BAND),
            state: State { choice, sel, busy: false, notice: Notice::None, draft: None },
            save: None,
        };
        page.rebuild();
        page
    }

    fn rebuild(&mut self) {
        let keep = self.form.selected_id().copied().unwrap_or(row_of(&self.state.choice));
        let shown = match (&self.state.draft, &self.state.choice) {
            // The caret, drawn in the text: Left and Right move it and there is no other mark of it.
            (Some(draft), _) => format!("{}|{}", &draft.text()[..draft.caret()], &draft.text()[draft.caret()..]),
            (None, RootBack::LaunchApp(id)) => id.clone(),
            _ => String::new(),
        };
        self.form.table.compact = false;
        self.form.set(root_back_form(&self.state.choice, &shown, self.state.busy), Some(&keep));
        self.form.table.list_focused = true;
    }

    fn view(&self) -> TableScreen<'_> {
        let copy = match self.state.notice {
            _ if self.state.busy => msg::settings_playback_saving(),
            Notice::Invalid => msg::settings_root_back_invalid(),
            Notice::SaveFailed => msg::settings_root_back_save_failed(),
            Notice::None => msg::settings_root_back_copy(),
        };
        TableScreen::new(Header::new(RouteLayout::screen(), Some(msg::settings_title()),
            msg::settings_root_back_title(), copy), &self.form.table, GroupId(0), self.entry).keyed(&self.form)
    }

    fn submit(&mut self, choice: RootBack, fx: &mut Effects<'_, InnerHost>) {
        if self.state.busy { return; }
        if choice == self.state.choice { return; }
        let (reply, receipt) = std::sync::mpsc::channel();
        self.save = Some((choice.clone(), receipt));
        self.state.busy = true;
        fx.push(Fx::App(AppFx::Preferences(PreferenceCmd::RootBack { choice, reply })));
    }

    fn keyboard(&self, up: bool, fx: &mut Effects<'_, InnerHost>) {
        fx.push(Fx::Deliver(MachineId::Instance(InstanceId(0)), Delivery::Keyboard { up }));
    }

    /// The draft's starting text: what is saved, so a small correction is not a retype.
    fn open_draft(&mut self) {
        let start = match &self.state.choice { RootBack::LaunchApp(id) => id.clone(), _ => String::new() };
        self.state.draft = Some(TextBuffer::new(start, usize::MAX));
        self.state.notice = Notice::None;
    }

    /// Leave the draft and lower the keyboard, whichever way the viewer finished.
    fn take_draft(&mut self, fx: &mut Effects<'_, InnerHost>) -> Option<String> {
        let draft = self.state.draft.take()?;
        self.keyboard(false, fx);
        Some(draft.text().trim().to_owned())
    }

    /// Save what was typed. Nothing typed is no answer rather than a wrong one.
    fn commit_text(&mut self, text: &str, fx: &mut Effects<'_, InnerHost>) {
        if text.is_empty() { return; }
        if nj_platform::tv::home::valid_app_id(text) {
            self.submit(RootBack::LaunchApp(text.to_owned()), fx);
        } else {
            self.state.notice = Notice::Invalid;
        }
    }

    fn activate(&mut self, key: u32, fx: &mut Effects<'_, InnerHost>) {
        let Some(id) = form_activate(&self.form, key, fx) else { return };
        if self.state.busy { return; }
        self.state.notice = Notice::None;
        match id {
            RowId::Home => self.submit(RootBack::PlatformHome, fx),
            RowId::Exit => self.submit(RootBack::Exit, fx),
            RowId::App => {
                self.open_draft();
                self.keyboard(true, fx);
            }
        }
        self.changed(fx);
    }

    fn changed(&mut self, fx: &mut Effects<'_, InnerHost>) {
        self.rebuild();
        fx.invalidate(nj_machine::present::Provenance::Input);
    }

    fn poll_save(&mut self) -> bool {
        let Some((choice, receipt)) = &self.save else { return false };
        let success = match receipt.try_recv() {
            Ok(success) => success,
            Err(TryRecvError::Disconnected) => false,
            Err(TryRecvError::Empty) => return false,
        };
        if success { self.state.choice = choice.clone(); }
        self.state.notice = if success { Notice::None } else { Notice::SaveFailed };
        self.state.busy = false;
        self.save = None;
        self.rebuild();
        true
    }

    /// A key while the keyboard is up. `None` is a key this page has no use for.
    fn editing_key(&mut self, key: Key, sym: u32, fx: &mut Effects<'_, InnerHost>) -> Option<Handled> {
        let edit = match (key, sym) {
            (Key::Left, _) => Some(TextEdit::Left),
            (Key::Right, _) => Some(TextEdit::Right),
            (_, crate::ui::consts::SDLK_BACKSPACE) => Some(TextEdit::Backspace),
            (_, crate::ui::consts::SDLK_CLEAR) => Some(TextEdit::Clear),
            _ => None,
        };
        if let (Some(edit), Some(draft)) = (edit, &mut self.state.draft) {
            draft.edit(&edit);
            self.changed(fx);
            return Some(Handled::Yes);
        }
        match key {
            Key::Back => {
                self.take_draft(fx);
                self.changed(fx);
                Some(Handled::Yes)
            }
            Key::Ok => {
                if let Some(text) = self.take_draft(fx) { self.commit_text(&text, fx); }
                self.changed(fx);
                Some(Handled::Yes)
            }
            Key::Up | Key::Down => {
                self.take_draft(fx);
                self.changed(fx);
                Some(Handled::No)
            }
            _ => None,
        }
    }
}

impl Machine<InnerHost> for RootBackPage {
    type Ev = ScreenEvent<InnerHost>;
    fn step(&mut self, ev: &Self::Ev, cx: &Cx<'_, InnerHost>, fx: &mut Effects<'_, InnerHost>) -> Handled {
        match ev {
            ScreenEvent::Mount => {
                // Opened on the saved choice, for the reason `LanguagePage` gives.
                fx.push(Fx::Deliver(
                    MachineId::Instance(InstanceId(0)),
                    Delivery::Screen(ScreenEvent::Enter(Enter::Fresh {
                        focus: FocusTarget::Elem(FocusKey { entry: self.entry, elem: self.state.sel.0 }),
                    })),
                ));
                Handled::Yes
            }
            ScreenEvent::Tick(t) => {
                if self.poll_save() { fx.invalidate(nj_machine::present::Provenance::Input); }
                self.form.table.update(t.dt(), RouteLayout::screen().sectioned_table().h);
                Handled::Yes
            }
            ScreenEvent::FocusMoved { to, .. } => {
                form_focus(&mut self.form, to.elem);
                if let Some(key) = self.form.key_at(self.form.table.sel.max(0) as usize) { self.state.sel = key; }
                Handled::Yes
            }
            ScreenEvent::Activate(key) => { self.activate(*key, fx); Handled::Yes }
            ScreenEvent::WillLeave(_) => {
                self.take_draft(fx);
                Handled::No
            }
            ScreenEvent::Input(InputEvent { kind, .. }) => match kind {
                // The panel closed by its own hand: its Enter is the only way some firmwares say
                // done, so this saves. BACK has already taken the draft by the time it arrives.
                InputKind::SystemKeyboard(false) => {
                    if let Some(draft) = self.state.draft.take() {
                        let text = draft.text().trim().to_owned();
                        self.commit_text(&text, fx);
                        self.changed(fx);
                    }
                    Handled::Yes
                }
                InputKind::SystemKeyboard(true) => {
                    if self.state.draft.is_none() && self.state.sel == RowId::App.key() {
                        self.open_draft();
                        self.changed(fx);
                    }
                    Handled::Yes
                }
                InputKind::Text(edit) => {
                    if let Some(draft) = &mut self.state.draft {
                        draft.edit(edit);
                        self.changed(fx);
                    }
                    Handled::Yes
                }
                InputKind::Key { key, sym, edge, .. } if *edge != Edge::Up && self.state.draft.is_some() => {
                    self.editing_key(*key, *sym, fx).unwrap_or(Handled::No)
                }
                InputKind::Key { key: Key::Right, at_edge: true, .. } => {
                    if let Some(key) = cx.focus.current.and_then(|k| form_right_target(&self.form, k.elem)) {
                        self.activate(key, fx);
                    }
                    Handled::Yes
                }
                _ => Handled::No,
            },
            _ => Handled::No,
        }
    }
}

crate::focusable_via_view!(RootBackPage, InnerHost, view);

impl Screen<InnerHost> for RootBackPage {
    fn name(&self) -> &'static str { "root-back" }
    fn state(&self) -> &dyn LogicalState { &self.state }
    fn crumb(&self, _cx: &Cx<'_, InnerHost>) -> Option<Cow<'_, str>> { Some(Cow::Borrowed(msg::settings_title())) }
    fn prepare(&mut self, _b: &mut Budget, _cx: &Cx<'_, InnerHost>) {}
    fn draw(&mut self, f: &mut DrawFrame<'_, '_, InnerHost>) {
        crate::ui::screen::Part::<InnerHost>::draw(&mut self.view(), f, Rect::FULL);
    }
    fn render(&self) -> RenderStrategy { RenderStrategy::Page }
    fn focus_source(&self) -> FocusSource { FocusSource::Engine }
    fn hit_source(&self) -> HitSource { HitSource::Engine }
}

#[cfg(test)]
#[path = "root_back_tests.rs"]
mod tests;
