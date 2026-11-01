//! The choice lands only on a durable receipt, and the id takes the keyboard's whole road.
use super::*;
use crate::ui::fixture::FixtureMeasure;
use nj_machine::machine::{FocusRead, InputOwner, PressRead, Source, Stamped, Tick};
use nj_machine::present::Present;
use nj_platform::i18n::{language_on_this_thread_for_test, SHIPPED};

static MEASURE: FixtureMeasure = FixtureMeasure;

const HOME: u32 = 0;
const EXIT: u32 = 1;
const APP: u32 = 2;

struct Saved;
impl Saved {
    fn set(choice: RootBack) -> Self {
        crate::route::restore_root_back(choice);
        Self
    }
}
impl Drop for Saved {
    fn drop(&mut self) { crate::route::restore_root_back(RootBack::PlatformHome); }
}

fn page() -> RootBackPage { RootBackPage::new(EntryId(0)) }

fn activate(page: &mut RootBackPage, row: u32) -> Vec<Stamped<InnerHost>> {
    let mut out = Vec::new();
    let mut present = Present::new();
    let mut fx = Effects::new(&mut out, MachineId::Instance(InstanceId(0)), &mut present);
    page.activate(row, &mut fx);
    out
}

fn input(page: &mut RootBackPage, kind: InputKind<u32>) -> (Handled, Vec<Stamped<InnerHost>>) {
    let mut out = Vec::new();
    let mut present = Present::new();
    let mut fx = Effects::new(&mut out, MachineId::Instance(InstanceId(0)), &mut present);
    let cx: Cx<'_, InnerHost> = Cx {
        views: crate::stores::browse::DirectoryView::empty_for_test(),
        tick: Tick::default(),
        measure: &MEASURE,
        press: PressRead::default(),
        focus: FocusRead::default(),
        owner: InputOwner::Entry(EntryId(0)),
    };
    let ev = ScreenEvent::Input(InputEvent { at: Tick { ms: 0, dt_us: 16_000 }, source: Source::Script, kind });
    let handled = page.step(&ev, &cx, &mut fx);
    (handled, out)
}

fn press(page: &mut RootBackPage, key: Key) -> (Handled, Vec<Stamped<InnerHost>>) {
    input(page, InputKind::Key { key, sym: 0, wcode: 0, edge: Edge::Down, at_edge: false })
}

fn typed(page: &mut RootBackPage, text: &str) {
    input(page, InputKind::Text(TextEdit::Commit(text.into())));
}

fn saves(effects: Vec<Stamped<InnerHost>>) -> Vec<(RootBack, std::sync::mpsc::Sender<bool>)> {
    effects.into_iter().filter_map(|e| match e.fx {
        Fx::App(AppFx::Preferences(PreferenceCmd::RootBack { choice, reply })) => Some((choice, reply)),
        _ => None,
    }).collect()
}

fn keyboard(effects: &[Stamped<InnerHost>]) -> Vec<bool> {
    effects.iter().filter_map(|e| match &e.fx {
        Fx::Deliver(_, Delivery::Keyboard { up }) => Some(*up),
        _ => None,
    }).collect()
}

#[test]
fn a_choice_lands_on_its_durable_receipt() {
    let _g = nj_base::testlock::serial();
    let _saved = Saved::set(RootBack::PlatformHome);
    let mut p = page();
    let mut sent = saves(activate(&mut p, EXIT));
    assert_eq!(sent.len(), 1);
    let (choice, reply) = sent.remove(0);
    assert_eq!(choice, RootBack::Exit);
    assert!(p.state.busy);
    assert_eq!(p.state.choice, RootBack::PlatformHome, "queue admission is not durable success");
    assert!(!p.poll_save());
    assert!(saves(activate(&mut p, HOME)).is_empty(), "one save at a time");
    reply.send(true).unwrap();
    assert!(p.poll_save());
    assert_eq!(p.state.choice, RootBack::Exit);
    assert!(!p.state.busy);
}

#[test]
fn a_failed_save_keeps_the_old_choice_and_says_so() {
    let _g = nj_base::testlock::serial();
    let _saved = Saved::set(RootBack::Exit);
    let mut p = page();
    let (_, reply) = saves(activate(&mut p, HOME)).remove(0);
    reply.send(false).unwrap();
    assert!(p.poll_save());
    assert_eq!(p.state.choice, RootBack::Exit);
    assert_eq!(p.state.notice, Notice::SaveFailed);
}

#[test]
fn ok_on_the_checked_choice_writes_nothing() {
    let _g = nj_base::testlock::serial();
    let _saved = Saved::set(RootBack::Exit);
    let mut p = page();
    assert!(saves(activate(&mut p, EXIT)).is_empty());
    assert!(!p.state.busy);
}

#[test]
fn a_typed_id_is_saved_by_ok_and_lowers_the_keyboard() {
    let _g = nj_base::testlock::serial();
    let _saved = Saved::set(RootBack::PlatformHome);
    let mut p = page();
    let opened = activate(&mut p, APP);
    assert_eq!(keyboard(&opened), [true]);
    assert!(saves(opened).is_empty(), "opening the keyboard is not an answer");
    typed(&mut p, "tld.my.launcher");
    let (handled, effects) = press(&mut p, Key::Ok);
    assert!(matches!(handled, Handled::Yes));
    assert_eq!(keyboard(&effects), [false]);
    assert_eq!(saves(effects).remove(0).0, RootBack::LaunchApp("tld.my.launcher".into()));
    assert!(p.state.draft.is_none());
}

#[test]
fn the_panel_closing_by_its_own_hand_saves_a_valid_draft_once() {
    let _g = nj_base::testlock::serial();
    let _saved = Saved::set(RootBack::PlatformHome);
    let mut p = page();
    activate(&mut p, APP);
    typed(&mut p, "tld.my.launcher");
    let (_, effects) = input(&mut p, InputKind::SystemKeyboard(false));
    assert_eq!(saves(effects).remove(0).0, RootBack::LaunchApp("tld.my.launcher".into()));
    let (_, again) = input(&mut p, InputKind::SystemKeyboard(false));
    assert!(saves(again).is_empty());
}

#[test]
fn back_leaves_the_keyboard_without_saving_and_keeps_the_page() {
    let _g = nj_base::testlock::serial();
    let _saved = Saved::set(RootBack::PlatformHome);
    let mut p = page();
    activate(&mut p, APP);
    typed(&mut p, "tld.my.launcher");
    let (handled, effects) = press(&mut p, Key::Back);
    assert!(matches!(handled, Handled::Yes), "this BACK belongs to the keyboard, not the stack");
    assert_eq!(keyboard(&effects), [false]);
    assert!(saves(effects).is_empty());
    let (_, closed) = input(&mut p, InputKind::SystemKeyboard(false));
    assert!(saves(closed).is_empty(), "the panel's closing edge after BACK is not a save");
    assert_eq!(p.state.choice, RootBack::PlatformHome);
}

#[test]
fn an_id_that_cannot_name_an_app_is_refused_and_nothing_typed_is_no_answer() {
    let _g = nj_base::testlock::serial();
    let _saved = Saved::set(RootBack::PlatformHome);
    let mut p = page();
    activate(&mut p, APP);
    typed(&mut p, "not an app");
    let (_, effects) = press(&mut p, Key::Ok);
    assert!(saves(effects).is_empty());
    assert_eq!(p.state.notice, Notice::Invalid);
    activate(&mut p, APP);
    assert_eq!(p.state.notice, Notice::None, "a new attempt clears it");
    let (_, effects) = press(&mut p, Key::Ok);
    assert!(saves(effects).is_empty());
    assert_eq!(p.state.notice, Notice::None);
}

#[test]
fn editing_starts_from_the_saved_id_and_the_caret_keys_edit_it() {
    let _g = nj_base::testlock::serial();
    let _saved = Saved::set(RootBack::LaunchApp("tld.a".into()));
    let mut p = page();
    activate(&mut p, APP);
    assert_eq!(p.state.draft.as_ref().unwrap().text(), "tld.a");
    press(&mut p, Key::Left);
    input(&mut p, InputKind::Text(TextEdit::Backspace));
    assert_eq!(p.state.draft.as_ref().unwrap().text(), "tlda");
    press(&mut p, Key::Right);
    typed(&mut p, "b");
    assert_eq!(p.state.draft.as_ref().unwrap().text(), "tldab");
}

#[test]
fn every_row_fits_in_every_language() {
    let _g = nj_base::testlock::serial();
    for language in SHIPPED {
        let _lang = language_on_this_thread_for_test(language);
        let tag = language.tag();
        for choice in [RootBack::PlatformHome, RootBack::Exit, RootBack::LaunchApp("tld.my.launcher".into())] {
            let _saved = Saved::set(choice);
            let out = page().form.table.app_fit_failures(RouteLayout::screen().sectioned_table().w, tag);
            crate::ui::table::assert_no_fit_failures(&out);
        }
    }
}
