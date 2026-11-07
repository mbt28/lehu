//! The controller against the recording host. Each test instantiates its
//! own screens: Slint's platform is per thread, and the test harness gives
//! every test a thread.

use slint::{ComponentHandle, Model};

use super::*;
use crate::host::{
    AudioOutput, BtDevice, CameraLevel, CameraRequest, KnownDevice, Power, Prompt, PromptKind,
};
use crate::keys::OsdKey;
use crate::test_host::{Call, TestHost};

const W: u32 = 800;
const H: u32 = 480;

/// A surface that remembers the bands it was given.
#[derive(Default)]
struct Sink(Vec<(u32, u32)>);

impl Surface for Sink {
    fn write_rows(&mut self, y0: u32, y1: u32, pixels: &[Rgb565Pixel]) {
        assert_eq!(pixels.len(), (y1 - y0) as usize * W as usize);
        self.0.push((y0, y1));
    }
}

fn osd(host: TestHost) -> Osd<TestHost> {
    let mut osd = Osd::new(host, W, H).expect("the screens start");
    tick(&mut osd); // the first tick: everything pushed, no ring
    osd
}

fn tick(osd: &mut Osd<TestHost>) -> bool {
    osd.tick(false, &mut Sink::default())
}

fn press(osd: &mut Osd<TestHost>, nav: OsdKey) {
    osd.key(nav, None, 1);
    tick(osd);
}

fn repeat(osd: &mut Osd<TestHost>, nav: OsdKey) {
    osd.key(nav, None, 2);
    tick(osd);
}

fn typed(osd: &mut Osd<TestHost>, c: char) {
    osd.key(OsdKey::None, Some(c), 1);
    tick(osd);
}

fn goto(osd: &mut Osd<TestHost>, screen: i32) {
    osd.ui.global::<Bridge<'_>>().set_screen(screen);
    tick(osd);
}

fn screen(osd: &Osd<TestHost>) -> i32 {
    osd.ui.global::<Bridge<'_>>().get_screen()
}

fn ring(osd: &Osd<TestHost>) -> String {
    osd.ui.global::<Bridge<'_>>().get_ring_row().to_string()
}

fn prompt_kind(osd: &Osd<TestHost>) -> i32 {
    osd.ui.global::<Bridge<'_>>().get_prompt_kind()
}

fn prompt_focus(osd: &Osd<TestHost>) -> i32 {
    osd.ui.global::<Bridge<'_>>().get_prompt_focus()
}

fn pick(osd: &Osd<TestHost>) -> (i32, i32) {
    let b = osd.ui.global::<Bridge<'_>>();
    (b.get_pick(), b.get_pick_choice())
}

fn pick_press(osd: &mut Osd<TestHost>, row: i32) {
    osd.ui.global::<Bridge<'_>>().invoke_pick_press(row);
    tick(osd);
}

fn editor_text(osd: &Osd<TestHost>) -> String {
    osd.ui.global::<Bridge<'_>>().get_editor_text().to_string()
}

fn edit(osd: &mut Osd<TestHost>, field: i32) {
    osd.ui.global::<Bridge<'_>>().invoke_edit_field(field);
    tick(osd);
}

fn osk_pos(osd: &Osd<TestHost>) -> (i32, i32) {
    let osk = osd.ui.global::<crate::Osk<'_>>();
    (osk.get_row(), osk.get_col())
}

fn advance(osd: &mut Osd<TestHost>, ms: u64) {
    osd.st.skew += ms;
}

fn question(title: &str) -> Prompt {
    Prompt {
        kind: PromptKind::Question,
        title: title.into(),
        code: String::new(),
        hint: "Pair?".into(),
        accept: "Pair".into(),
    }
}

fn bt_question(osd: &mut Osd<TestHost>) {
    let host = osd.host_mut();
    host.bt_prompt = question("Keyboard");
    host.bt_gen += 1;
    tick(osd);
    assert_eq!(prompt_kind(osd), 2);
}

fn with_camera() -> TestHost {
    let mut host = TestHost::new();
    host.camera_present = true;
    host.camera_inputs = 2;
    host.camera_standard = Some("Auto".into());
    host.camera_levels = vec![
        CameraLevel {
            label: "Brightness".into(),
            value: "50".into(),
        },
        CameraLevel {
            label: "Contrast".into(),
            value: "60".into(),
        },
    ];
    host
}

// ── keys on the list screens ─────────────────────────────────────────────

#[test]
fn tab_and_backtab_move_the_ring_and_enter_activates() {
    let mut host = TestHost::new();
    host.backgrounded = true;
    let mut osd = osd(host);
    assert_eq!(ring(&osd), "");
    press(&mut osd, OsdKey::Down); // Tab from nothing: the first tile, the phone's
    assert_eq!(ring(&osd), "resume");
    press(&mut osd, OsdKey::Next); // a detent is a Tab too
    assert_eq!(ring(&osd), "phones");
    press(&mut osd, OsdKey::Up); // Backtab
    assert_eq!(ring(&osd), "resume");
    press(&mut osd, OsdKey::Ok); // Return on the focused tile
    assert!(osd.host().has(&Call::RequestResume));
}

#[test]
fn back_walks_the_chain_and_leaves_the_home_screen_for_the_session() {
    let mut host = TestHost::new();
    host.backgrounded = true;
    host.bt_paired = vec![BtDevice {
        id: "/dev/b".into(),
        name: "Remote".into(),
        ..Default::default()
    }];
    let mut osd = osd(host);
    osd.ui
        .global::<Bridge<'_>>()
        .invoke_bt_open("/dev/b".into());
    goto(&mut osd, BT_DEVICE);
    assert_eq!(screen(&osd), BT_DEVICE);
    for expected in [BLUETOOTH, 3, SETTINGS, HOME] {
        press(&mut osd, OsdKey::Back);
        assert_eq!(screen(&osd), expected);
    }
    assert!(!osd.host().has(&Call::RequestResume));
    press(&mut osd, OsdKey::Back);
    assert_eq!(osd.host().count(&Call::RequestResume), 1);
    // Left and Erase are Back too; a repeat is not.
    goto(&mut osd, VIDEO);
    press(&mut osd, OsdKey::Left);
    assert_eq!(screen(&osd), SETTINGS);
    repeat(&mut osd, OsdKey::Erase);
    assert_eq!(screen(&osd), SETTINGS);
    press(&mut osd, OsdKey::Erase);
    assert_eq!(screen(&osd), HOME);
}

#[test]
fn home_and_menu() {
    let mut osd = osd(TestHost::new());
    goto(&mut osd, 3);
    press(&mut osd, OsdKey::Home);
    assert_eq!(screen(&osd), HOME);
    goto(&mut osd, SETTINGS);
    press(&mut osd, OsdKey::Menu);
    assert_eq!(screen(&osd), HOME);
    // Menu on the home screen: nothing to resume, nothing happens.
    press(&mut osd, OsdKey::Menu);
    assert!(!osd.host().has(&Call::RequestResume));
    osd.host_mut().backgrounded = true;
    tick(&mut osd);
    press(&mut osd, OsdKey::Menu);
    assert!(osd.host().has(&Call::RequestResume));
    // Home closes the editor and cancels a dialog on the way.
    goto(&mut osd, 3);
    edit(&mut osd, 0);
    assert_eq!(osd.st.editing, 0);
    press(&mut osd, OsdKey::Home);
    assert_eq!(osd.st.editing, -1);
    assert_eq!(screen(&osd), HOME);
    goto(&mut osd, 3);
    bt_question(&mut osd);
    press(&mut osd, OsdKey::Home);
    assert!(osd.host().has(&Call::BtPromptCancel));
    assert_eq!(screen(&osd), HOME);
}

#[test]
fn the_ring_is_reseeded_only_for_a_recent_keyboard_user() {
    let mut host = TestHost::new();
    host.backgrounded = true;
    let mut osd = osd(host);
    // Touch-style navigation: no key was ever used, so no ring.
    goto(&mut osd, SETTINGS);
    goto(&mut osd, HOME);
    assert_eq!(ring(&osd), "");
    // A key was used: every switch puts the ring on the first row.
    press(&mut osd, OsdKey::Down);
    goto(&mut osd, SETTINGS);
    goto(&mut osd, HOME);
    assert_eq!(ring(&osd), "resume");
    // Ten seconds later the keyboard is forgotten again.
    advance(&mut osd, KEY_RECENT_MS + 1);
    goto(&mut osd, SETTINGS);
    goto(&mut osd, HOME);
    assert_eq!(ring(&osd), "");
}

#[test]
fn a_ringed_row_that_goes_away_puts_the_ring_back_on_the_first_row() {
    let mut host = TestHost::new();
    host.backgrounded = true;
    let mut osd = osd(host);
    press(&mut osd, OsdKey::Down);
    assert_eq!(ring(&osd), "resume");
    osd.host_mut().backgrounded = false;
    tick(&mut osd);
    assert_eq!(ring(&osd), "");
    assert!(osd.st.ring_lost);
    tick(&mut osd); // Tab from nothing lands on the first tile: Connect
    assert!(!osd.st.ring_lost);
    assert_eq!(ring(&osd), "connect");
    // The session back: its tile takes Connect's place, and the ring.
    osd.host_mut().backgrounded = true;
    tick(&mut osd);
    assert_eq!(ring(&osd), "");
    tick(&mut osd);
    assert_eq!(ring(&osd), "resume");
}

#[test]
fn a_tap_reaches_a_row_even_between_two_ticks() {
    let mut osd = osd(TestHost::new());
    // Home: the bar, then the tiles; Settings is the bottom right one.
    osd.pointer(600, 370, true);
    tick(&mut osd);
    osd.pointer(600, 370, false);
    tick(&mut osd);
    assert_eq!(screen(&osd), SETTINGS);
    // Settings: the back key, at the bar's left.
    osd.pointer(32, 28, true);
    osd.pointer(32, 28, false);
    tick(&mut osd);
    assert_eq!(screen(&osd), HOME);
    assert_eq!(ring(&osd), ""); // touch never shows a ring
}

// ── rows picked in place ─────────────────────────────────────────────────

#[test]
fn the_phone_row_opens_the_devices_screen() {
    let mut osd = osd(with_phones());
    goto(&mut osd, SETTINGS);
    // By the knob: Back, Volume, Night, Phones.
    for _ in 0..4 {
        press(&mut osd, OsdKey::Next);
    }
    assert_eq!(ring(&osd), "phones");
    press(&mut osd, OsdKey::Ok);
    assert_eq!(screen(&osd), DEVICES);
    assert_eq!(ring(&osd), "back");
    press(&mut osd, OsdKey::Back);
    assert_eq!(screen(&osd), SETTINGS);
    // Back where it was opened from, the ring on the row that opened it.
    assert_eq!(ring(&osd), "phones");
    // By touch: the third row, from y 224.
    advance(&mut osd, KEY_RECENT_MS);
    osd.pointer(400, 260, true);
    osd.pointer(400, 260, false);
    tick(&mut osd);
    assert_eq!(screen(&osd), DEVICES);
    assert_eq!(ring(&osd), "");
}

/// The home tiles' words: the phone's tile (its protocol, the phone and
/// the link) and the note under Connect.
fn home_words(osd: &Osd<TestHost>) -> (String, String, String) {
    let b = osd.ui.global::<Bridge<'_>>();
    (
        b.get_phone_title().to_string(),
        b.get_phone_detail().to_string(),
        b.get_status_note().to_string(),
    )
}

#[test]
fn the_phones_tile_follows_the_session_and_its_link() {
    let mut osd = osd(TestHost::new());
    assert_eq!(home_words(&osd), ("".into(), "".into(), "".into()));
    assert!(!osd.ui.global::<Bridge<'_>>().get_session_waiting());
    let host = osd.host_mut();
    host.status = "CarPlay: Alex\u{2019}s iPhone".into();
    host.backgrounded = true;
    host.link_usb = true;
    tick(&mut osd);
    assert!(osd.ui.global::<Bridge<'_>>().get_session_waiting());
    assert_eq!(
        home_words(&osd),
        ("CarPlay".into(), "Alex's iPhone - wired".into(), "".into())
    );
    // A cable wins over Wi-Fi.
    osd.host_mut().link_wifi = true;
    tick(&mut osd);
    assert_eq!(home_words(&osd).1, "Alex's iPhone - wired");
    osd.host_mut().link_usb = false;
    tick(&mut osd);
    assert_eq!(home_words(&osd).1, "Alex's iPhone - wireless");
    // No session: what a phone does, under Connect a phone; nothing for
    // no phone at all.
    let host = osd.host_mut();
    host.status = "iPhone plugged in".into();
    host.backgrounded = false;
    host.link_wifi = false;
    tick(&mut osd);
    assert_eq!(
        home_words(&osd),
        ("".into(), "".into(), "iPhone plugged in".into())
    );
    osd.host_mut().status = "No phone".into();
    tick(&mut osd);
    assert_eq!(home_words(&osd), ("".into(), "".into(), "".into()));
    // An unchanged home writes nothing: the frame stays as it is.
    let mut sink = Sink::default();
    assert!(!osd.tick(false, &mut sink));
    assert!(sink.0.is_empty());
}

#[test]
fn night_is_taken_at_once_and_a_tap_takes_the_tapped_option() {
    let mut osd = osd(TestHost::new());
    goto(&mut osd, SETTINGS);
    pick_press(&mut osd, 2);
    assert_eq!(pick(&osd), (2, 2)); // Auto, the default
    press(&mut osd, OsdKey::Up); // back along the options
    press(&mut osd, OsdKey::Ok);
    assert_eq!(
        osd.host().persisted("display.night-mode"),
        Some(&toml::Value::String("night".into()))
    );
    assert!(osd.host().has(&Call::NightModeChanged));
    assert_eq!(
        osd.ui.global::<Bridge<'_>>().get_night_label().as_str(),
        "Night"
    );
    pick_press(&mut osd, 2);
    osd.ui.global::<Bridge<'_>>().invoke_pick_option(0);
    tick(&mut osd);
    assert_eq!(
        osd.host().persisted("display.night-mode"),
        Some(&toml::Value::String("day".into()))
    );
    assert_eq!(pick(&osd), (0, 0));
}

#[test]
fn power_opens_on_cancel_and_a_double_press_never_takes_the_unit_down() {
    let mut osd = osd(TestHost::new());
    goto(&mut osd, SETTINGS);
    pick_press(&mut osd, 3);
    assert_eq!(pick(&osd), (3, 2)); // Cancel
    press(&mut osd, OsdKey::Ok); // takes Cancel: nothing asked
    assert_eq!(pick(&osd), (0, 2));
    assert_eq!(prompt_kind(&osd), 0);

    pick_press(&mut osd, 3);
    press(&mut osd, OsdKey::Left); // Power off
    press(&mut osd, OsdKey::Ok);
    assert_eq!(prompt_kind(&osd), 2);
    assert_eq!(
        osd.ui.global::<Bridge<'_>>().get_prompt_title().as_str(),
        "Shut down the head unit?"
    );
    assert!(osd.ui.global::<Bridge<'_>>().get_prompt_danger());
    // Keys in use: the ring is already on Cancel.
    assert_eq!(prompt_focus(&osd), 1);
    press(&mut osd, OsdKey::Ok); // Cancel
    assert_eq!(prompt_kind(&osd), 0);
    assert!(!osd.host().has(&Call::RequestPower(Power::Shutdown)));

    pick_press(&mut osd, 3);
    press(&mut osd, OsdKey::Left);
    press(&mut osd, OsdKey::Left); // Reboot
    press(&mut osd, OsdKey::Ok);
    assert_eq!(prompt_focus(&osd), 1);
    press(&mut osd, OsdKey::Left); // Cancel is the leftmost
    assert_eq!(prompt_focus(&osd), 1);
    press(&mut osd, OsdKey::Right); // to Reboot
    assert_eq!(prompt_focus(&osd), 0);
    press(&mut osd, OsdKey::Ok);
    assert!(osd.host().has(&Call::RequestPower(Power::Reboot)));
    assert_eq!(prompt_kind(&osd), 0);

    // Opened by touch, the dialog shows no ring, and a press with none
    // only shows it: a double press never takes the unit down.
    advance(&mut osd, KEY_RECENT_MS);
    pick_press(&mut osd, 3);
    osd.ui.global::<Bridge<'_>>().invoke_pick_option(1);
    tick(&mut osd);
    assert_eq!(prompt_focus(&osd), -1);
    press(&mut osd, OsdKey::Ok);
    assert_eq!(prompt_focus(&osd), 1);
    press(&mut osd, OsdKey::Ok);
    assert!(!osd.host().has(&Call::RequestPower(Power::Shutdown)));
}

#[test]
fn the_power_question_does_not_wait_behind_the_video() {
    let mut osd = osd(TestHost::new());
    goto(&mut osd, SETTINGS);
    pick_press(&mut osd, 3);
    press(&mut osd, OsdKey::Left);
    press(&mut osd, OsdKey::Ok);
    assert_eq!(prompt_kind(&osd), 2);
    osd.hidden();
    tick(&mut osd);
    assert_eq!(prompt_kind(&osd), 0);
    assert!(!osd.host().has(&Call::RequestPower(Power::Shutdown)));
    // Bluetooth's dialog stays.
    bt_question(&mut osd);
    osd.hidden();
    tick(&mut osd);
    assert_eq!(prompt_kind(&osd), 2);
    assert!(!osd.host().has(&Call::BtPromptCancel));
}

#[test]
fn the_level_rows_turn_and_the_buffer_is_saved_once_turning_stops() {
    let mut osd = osd(TestHost::new());
    goto(&mut osd, SETTINGS);
    pick_press(&mut osd, 4);
    assert_eq!(pick(&osd), (4, 0));
    press(&mut osd, OsdKey::Up);
    assert!(osd.host().has(&Call::VolumeStep(1)));
    press(&mut osd, OsdKey::Down);
    press(&mut osd, OsdKey::Prev);
    assert_eq!(osd.host().count(&Call::VolumeStep(-1)), 2);
    press(&mut osd, OsdKey::Back);
    assert_eq!(pick(&osd), (0, 0));

    // The buffer is on the Audio screen.
    goto(&mut osd, AUDIO);
    pick_press(&mut osd, 5);
    press(&mut osd, OsdKey::Up);
    assert!(osd.host().has(&Call::ApplyAudioBuffer(550)));
    assert_eq!(
        osd.ui.global::<Bridge<'_>>().get_buffer_label().as_str(),
        "550 ms"
    );
    assert!(osd.host().persisted("audio.buffer-ms").is_none());
    advance(&mut osd, BUFFER_SAVE_MS);
    tick(&mut osd);
    assert_eq!(
        osd.host().persisted("audio.buffer-ms"),
        Some(&toml::Value::Integer(550))
    );
    // Enter ends it and saves what is left unsaved.
    press(&mut osd, OsdKey::Down);
    press(&mut osd, OsdKey::Ok);
    assert_eq!(
        osd.host().persisted("audio.buffer-ms"),
        Some(&toml::Value::Integer(500))
    );
    assert_eq!(pick(&osd), (0, 0));
}

#[test]
fn a_pick_ends_when_left_alone_or_off_its_screen() {
    let mut osd = osd(TestHost::new());
    goto(&mut osd, SETTINGS);
    pick_press(&mut osd, 2);
    advance(&mut osd, PICK_MS / 2);
    tick(&mut osd);
    assert_eq!(pick(&osd).0, 2);
    advance(&mut osd, PICK_MS / 2);
    tick(&mut osd);
    assert_eq!(pick(&osd).0, 0);
    pick_press(&mut osd, 2);
    goto(&mut osd, HOME);
    assert_eq!(pick(&osd).0, 0);
}

// ── the dialog ───────────────────────────────────────────────────────────

#[test]
fn a_knob_goes_round_the_dialog_and_enter_accepts_without_a_ring() {
    let mut osd = osd(TestHost::new());
    bt_question(&mut osd);
    assert_eq!(prompt_focus(&osd), -1);
    press(&mut osd, OsdKey::Next);
    assert_eq!(prompt_focus(&osd), 0);
    press(&mut osd, OsdKey::Next);
    assert_eq!(prompt_focus(&osd), 1);
    press(&mut osd, OsdKey::Next);
    assert_eq!(prompt_focus(&osd), 0);
    press(&mut osd, OsdKey::Prev);
    assert_eq!(prompt_focus(&osd), 1);
    // Cancel left, the accept right.
    press(&mut osd, OsdKey::Right);
    assert_eq!(prompt_focus(&osd), 0);
    press(&mut osd, OsdKey::Left);
    assert_eq!(prompt_focus(&osd), 1);
    press(&mut osd, OsdKey::Left);
    assert_eq!(prompt_focus(&osd), 1);
    assert!(!osd.ui.global::<Bridge<'_>>().get_prompt_danger());
    press(&mut osd, OsdKey::Ok);
    assert!(osd.host().has(&Call::BtPromptCancel));
    assert_eq!(prompt_kind(&osd), 0);

    bt_question(&mut osd);
    press(&mut osd, OsdKey::Ok);
    assert!(osd.host().has(&Call::BtPromptAccept));

    // A code dialog has only Cancel.
    let host = osd.host_mut();
    host.bt_prompt = Prompt {
        kind: PromptKind::Code,
        title: "Type on the keyboard".into(),
        code: "123456".into(),
        ..Default::default()
    };
    host.bt_gen += 1;
    tick(&mut osd);
    assert_eq!(prompt_kind(&osd), 1);
    press(&mut osd, OsdKey::Next);
    assert_eq!(prompt_focus(&osd), 1);
    press(&mut osd, OsdKey::Left);
    assert_eq!(prompt_focus(&osd), 1);
    press(&mut osd, OsdKey::Back);
    assert_eq!(osd.host().count(&Call::BtPromptCancel), 2);
}

#[test]
fn a_dialog_over_the_camera_view_leaves_it() {
    let mut osd = osd(with_camera());
    goto(&mut osd, VIDEO);
    goto(&mut osd, CAMERA);
    bt_question(&mut osd);
    tick(&mut osd); // the dialog is pumped last; the next tick sees it
    assert_eq!(screen(&osd), VIDEO);
}

// ── the editor and the on-screen keyboard ────────────────────────────────

#[test]
fn typing_arrows_and_the_knob_in_the_editor() {
    let mut osd = osd(TestHost::new());
    goto(&mut osd, 3);
    edit(&mut osd, 0);
    assert_eq!(osd.st.editing, 0);
    assert_eq!(
        osd.ui.global::<Bridge<'_>>().get_editor_label().as_str(),
        "Name"
    );
    assert_eq!(editor_text(&osd), "LEHU");
    typed(&mut osd, 'a');
    assert_eq!(editor_text(&osd), "LEHUa");
    press(&mut osd, OsdKey::Erase);
    assert_eq!(editor_text(&osd), "LEHU");
    // The first arrow only shows the ring, on the first letter.
    press(&mut osd, OsdKey::Down);
    assert_eq!(osk_pos(&osd), (1, 0));
    press(&mut osd, OsdKey::Right);
    assert_eq!(osk_pos(&osd), (1, 1));
    press(&mut osd, OsdKey::Ok); // presses the ringed key
    assert_eq!(editor_text(&osd), "LEHUw");
    typed(&mut osd, 'x'); // typing hides the ring
    assert_eq!(osk_pos(&osd).0, -1);
    assert_eq!(editor_text(&osd), "LEHUwx");
    press(&mut osd, OsdKey::Next);
    assert_eq!(osk_pos(&osd), (1, 0));
    press(&mut osd, OsdKey::Prev); // wraps back to Cancel
    assert_eq!(osk_pos(&osd), (0, 0));
    press(&mut osd, OsdKey::Ok);
    assert_eq!(osd.st.editing, -1);
    assert!(!osd.host().has(&Call::ConfigureAccessPoint(
        "LEHUwx".into(),
        "lehu1234".into()
    )));
}

#[test]
fn the_editor_validates_before_it_saves() {
    let mut osd = osd(TestHost::new());
    goto(&mut osd, 3);
    edit(&mut osd, 0);
    for _ in 0..4 {
        press(&mut osd, OsdKey::Erase);
    }
    assert_eq!(editor_text(&osd), "");
    press(&mut osd, OsdKey::Ok);
    assert_eq!(osd.st.editing, 0);
    assert_eq!(
        osd.ui.global::<Bridge<'_>>().get_editor_error().as_str(),
        "Needs 1-32 characters"
    );
    for c in "Car".chars() {
        typed(&mut osd, c);
    }
    press(&mut osd, OsdKey::Ok);
    assert_eq!(osd.st.editing, -1);
    // Written on the tick after the one that says "Restarting...".
    assert!(
        !osd.host()
            .has(&Call::ConfigureAccessPoint("Car".into(), "lehu1234".into()))
    );
    tick(&mut osd);
    assert!(
        osd.host()
            .has(&Call::ConfigureAccessPoint("Car".into(), "lehu1234".into()))
    );
    assert_eq!(
        osd.ui.global::<Bridge<'_>>().get_wifi_ssid().as_str(),
        "Car"
    );
    // The limit holds while typing.
    edit(&mut osd, 0);
    for _ in 0..40 {
        typed(&mut osd, 'z');
    }
    assert_eq!(editor_text(&osd).len(), 32);
    press(&mut osd, OsdKey::Back);
    assert_eq!(osd.st.editing, -1);
    // The Bluetooth name is a setting; the program renames the adapter.
    edit(&mut osd, 2);
    typed(&mut osd, '2');
    press(&mut osd, OsdKey::Ok);
    assert_eq!(
        osd.host().persisted("bluetooth.name"),
        Some(&toml::Value::String("LEHU2".into()))
    );
}

#[test]
fn a_held_backspace_deletes_only_when_pressed_in_the_editor() {
    let mut osd = osd(TestHost::new());
    goto(&mut osd, 3);
    edit(&mut osd, 0);
    bt_question(&mut osd);
    press(&mut osd, OsdKey::Erase); // cancels the dialog
    assert_eq!(prompt_kind(&osd), 0);
    assert_eq!(osd.st.editing, 0);
    repeat(&mut osd, OsdKey::Erase); // still held: the text stays
    assert_eq!(editor_text(&osd), "LEHU");
    press(&mut osd, OsdKey::Erase);
    repeat(&mut osd, OsdKey::Erase);
    assert_eq!(editor_text(&osd), "LE");
}

// ── the pump ─────────────────────────────────────────────────────────────

#[test]
fn the_first_tick_pushes_the_whole_frame_and_an_unchanged_tick_nothing() {
    let mut osd = Osd::new(TestHost::new(), W, H).expect("the screens start");
    let mut sink = Sink::default();
    assert!(osd.tick(false, &mut sink));
    assert_eq!(sink.0, vec![(0, H)]);
    let mut sink = Sink::default();
    assert!(!osd.tick(false, &mut sink));
    assert!(!osd.tick(false, &mut sink));
    assert!(sink.0.is_empty());
    // A status change repaints the band of its words, not the frame.
    osd.host_mut().status = "Alex\u{2019}s iPhone plugged in".into();
    assert!(osd.tick(false, &mut sink));
    assert_eq!(sink.0.len(), 1);
    let (y0, y1) = sink.0[0];
    assert!(y1 > y0 && y1 - y0 < H / 4, "band {y0}..{y1}");
    assert_eq!(
        osd.ui.global::<Bridge<'_>>().get_status_note().as_str(),
        "Alex's iPhone plugged in"
    );
    assert!(!osd.tick(false, &mut sink));
}

#[test]
fn invalidate_pushes_everything_but_waits_over_video() {
    let mut osd = osd(TestHost::new());
    osd.invalidate();
    let mut sink = Sink::default();
    assert!(!osd.tick(true, &mut sink));
    assert!(sink.0.is_empty());
    assert!(osd.tick(false, &mut sink));
    assert_eq!(sink.0, vec![(0, H)]);
}

#[test]
fn settings_and_links_reach_the_screens_on_change() {
    let mut osd = osd(TestHost::new());
    let host = osd.host_mut();
    host.link_usb = true;
    host.bt_phone = true;
    host.bt_gen += 1;
    host.settings.camera.key = 88;
    host.settings.camera.fast_switch = true;
    tick(&mut osd);
    let b = osd.ui.global::<Bridge<'_>>();
    assert!(b.get_link_usb());
    assert!(!b.get_link_wifi());
    assert!(b.get_link_bt());
    assert_eq!(b.get_camera_key_label().as_str(), "F12");
    assert!(b.get_camera_fastswitch());
    assert_eq!(b.get_fps_label().as_str(), "30 fps");
    assert_eq!(b.get_night_label().as_str(), "Auto");
    assert_eq!(b.get_volume_label().as_str(), "60%");
    assert_eq!(b.get_bt_name().as_str(), "LEHU");
    assert!(b.get_bt_available());
    let host = osd.host_mut();
    host.bt_available = false;
    host.bt_gen += 1;
    tick(&mut osd);
    assert!(!osd.ui.global::<Bridge<'_>>().get_bt_available());
}

#[test]
fn the_video_rows_cycle_the_frame_rate_and_the_fast_switch() {
    let mut osd = osd(with_camera());
    goto(&mut osd, VIDEO);
    assert!(osd.ui.global::<Bridge<'_>>().get_camera_available());
    // A tap on 60 fps takes it: the segment row starts and takes at once.
    osd.ui.global::<Bridge<'_>>().invoke_pick_press(16);
    osd.ui.global::<Bridge<'_>>().invoke_pick_option(1);
    tick(&mut osd);
    assert_eq!(
        osd.host().persisted("android-auto.fps"),
        Some(&toml::Value::Integer(60))
    );
    assert_eq!(pick(&osd).0, 0);
    let b = osd.ui.global::<Bridge<'_>>();
    assert_eq!(b.get_fps_label().as_str(), "60 fps");
    assert_eq!(b.get_fps_choice(), 1);
    // By the knob: Enter starts on the rate in force, a turn moves, Enter
    // takes.
    pick_press(&mut osd, 16);
    assert_eq!(pick(&osd), (16, 1));
    press(&mut osd, OsdKey::Prev);
    assert_eq!(pick(&osd), (16, 0));
    press(&mut osd, OsdKey::Ok);
    assert_eq!(
        osd.host().persisted("android-auto.fps"),
        Some(&toml::Value::Integer(30))
    );
    assert_eq!(osd.ui.global::<Bridge<'_>>().get_fps_choice(), 0);
    osd.ui
        .global::<Bridge<'_>>()
        .invoke_camera_fastswitch_toggle();
    tick(&mut osd);
    assert_eq!(
        osd.host().persisted("camera.fast-switch"),
        Some(&toml::Value::Boolean(true))
    );
    // The camera key: asked for, learned, and cleared.
    osd.ui.global::<Bridge<'_>>().invoke_camera_key();
    tick(&mut osd);
    assert!(osd.host().has(&Call::SetKeyLearning(true)));
    assert_eq!(prompt_kind(&osd), 2);
    osd.host_mut().learned_key = Some(59);
    tick(&mut osd);
    assert_eq!(prompt_kind(&osd), 0);
    assert_eq!(
        osd.host().persisted("camera.key"),
        Some(&toml::Value::Integer(59))
    );
    assert_eq!(
        osd.ui
            .global::<Bridge<'_>>()
            .get_camera_key_label()
            .as_str(),
        "F1"
    );
    osd.ui.global::<Bridge<'_>>().invoke_camera_key();
    tick(&mut osd);
    assert!(!osd.ui.global::<Bridge<'_>>().get_prompt_danger());
    assert_eq!(prompt_focus(&osd), 1); // keys in use: on Cancel
    press(&mut osd, OsdKey::Right);
    press(&mut osd, OsdKey::Ok); // Clear
    assert!(osd.host().has(&Call::SetKeyLearning(false)));
    assert_eq!(
        osd.host().persisted("camera.key"),
        Some(&toml::Value::Integer(0))
    );
}

// ── the volume bar ───────────────────────────────────────────────────────

#[test]
fn the_volume_bar_shows_for_two_seconds_after_a_change() {
    let mut osd = osd(TestHost::new());
    assert_eq!(osd.over_video(), None); // the first look is no change
    osd.host_mut().volume_gen += 1;
    // Pending, not yet ticked in: the window is already asked for.
    let bar = osd.over_video().expect("the bar");
    assert_eq!(
        bar,
        Rect {
            x: 160,
            y: 392,
            width: 480,
            height: 64
        }
    );
    tick(&mut osd);
    assert!(osd.ui.global::<Bridge<'_>>().get_volume_bar());
    advance(&mut osd, VOLUME_BAR_MS / 2);
    assert!(osd.over_video().is_some());
    advance(&mut osd, VOLUME_BAR_MS / 2);
    assert_eq!(osd.over_video(), None);
    tick(&mut osd);
    assert!(!osd.ui.global::<Bridge<'_>>().get_volume_bar());
    // 0% is muted.
    let host = osd.host_mut();
    host.volume_level = 0;
    host.volume_gen += 1;
    tick(&mut osd);
    let b = osd.ui.global::<Bridge<'_>>();
    assert_eq!(b.get_volume_label().as_str(), "Muted");
    assert!(b.get_volume_muted());
}

// ── the camera view ──────────────────────────────────────────────────────

#[test]
fn the_camera_column_has_two_pages_and_keys_walk_them() {
    let mut osd = osd(with_camera());
    goto(&mut osd, VIDEO);
    goto(&mut osd, CAMERA);
    assert!(osd.host().has(&Call::SetCameraRunning(true)));
    let rows = osd.st.cam_rows.clone();
    let labels = |rows: &VecModel<CameraRow>| -> Vec<String> {
        (0..rows.row_count())
            .filter_map(|i| rows.row_data(i))
            .map(|r| r.label.to_string())
            .collect()
    };
    assert_eq!(
        labels(&rows),
        ["Back", "Input 1", "Input 2", "Standard", "Picture"]
    );
    assert_eq!(rows.row_data(1).map(|r| r.active), Some(true));
    assert_eq!(
        osd.over_video(),
        Some(Rect {
            x: 620,
            y: 0,
            width: 180,
            height: 5 * 44 + 12
        })
    );
    let b = osd.ui.global::<Bridge<'_>>();
    assert_eq!((b.get_camera_page(), b.get_camera_row()), (0, 0));

    press(&mut osd, OsdKey::Down);
    press(&mut osd, OsdKey::Ok); // Input 1
    assert!(osd.host().has(&Call::CameraSelectInput(0)));
    press(&mut osd, OsdKey::Next); // turning never changes the input
    press(&mut osd, OsdKey::Next); // Standard
    assert!(!osd.host().has(&Call::CameraCycleStandard(1)));
    press(&mut osd, OsdKey::Right);
    assert!(osd.host().has(&Call::CameraCycleStandard(1)));
    press(&mut osd, OsdKey::Down); // Picture
    press(&mut osd, OsdKey::Ok);
    assert_eq!(labels(&rows), ["Back", "Brightness", "Contrast"]);
    let b = osd.ui.global::<Bridge<'_>>();
    assert_eq!((b.get_camera_page(), b.get_camera_row()), (1, 1));
    press(&mut osd, OsdKey::Ok); // start turning Brightness
    assert!(osd.ui.global::<Bridge<'_>>().get_camera_editing());
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Up);
    press(&mut osd, OsdKey::Left);
    assert_eq!(osd.host().count(&Call::CameraAdjustLevel(0, 1)), 2);
    assert_eq!(osd.host().count(&Call::CameraAdjustLevel(0, -1)), 1);
    press(&mut osd, OsdKey::Back); // stop turning
    assert!(!osd.ui.global::<Bridge<'_>>().get_camera_editing());
    press(&mut osd, OsdKey::Back); // the menu, the ring on Picture
    let b = osd.ui.global::<Bridge<'_>>();
    assert_eq!((b.get_camera_page(), b.get_camera_row()), (0, 4));
    press(&mut osd, OsdKey::Back); // out
    assert_eq!(screen(&osd), VIDEO);
    assert!(osd.host().has(&Call::SetCameraRunning(false)));
}

#[test]
fn the_camera_column_hides_and_comes_back() {
    let mut osd = osd(with_camera());
    goto(&mut osd, VIDEO);
    goto(&mut osd, CAMERA);
    advance(&mut osd, CAMERA_PANEL_MS);
    tick(&mut osd);
    assert_eq!(osd.over_video(), None);
    press(&mut osd, OsdKey::Down); // only brings the column back
    assert!(osd.over_video().is_some());
    assert_eq!(osd.ui.global::<Bridge<'_>>().get_camera_row(), 0);
    // A tap on the picture hides it; a touch while hidden only brings it
    // back, and that tap reaches nothing.
    osd.ui.global::<Bridge<'_>>().invoke_camera_picture();
    tick(&mut osd);
    assert_eq!(osd.over_video(), None);
    osd.pointer(100, 100, true);
    tick(&mut osd);
    assert!(osd.st.touch_swallow);
    osd.pointer(100, 100, false);
    tick(&mut osd);
    assert!(osd.over_video().is_some());
    assert_eq!(screen(&osd), CAMERA);
    // Hidden, Back leaves.
    advance(&mut osd, CAMERA_PANEL_MS);
    tick(&mut osd);
    press(&mut osd, OsdKey::Back);
    assert_eq!(screen(&osd), VIDEO);
}

#[test]
fn the_camera_key_switches_from_anywhere_without_the_column() {
    let mut osd = osd(with_camera());
    goto(&mut osd, SETTINGS);
    osd.host_mut().camera_request = CameraRequest::Toggle;
    osd.camera_requests();
    assert_eq!(screen(&osd), CAMERA);
    assert!(osd.host().has(&Call::SetCameraRunning(true)));
    tick(&mut osd);
    assert_eq!(osd.over_video(), None);
    let stops = osd.host().count(&Call::SetCameraRunning(false));
    osd.host_mut().camera_request = CameraRequest::Toggle;
    osd.camera_requests();
    assert_eq!(screen(&osd), SETTINGS);
    // Stopped at once, not only when the screens are next ticked.
    assert_eq!(osd.host().count(&Call::SetCameraRunning(false)), stops + 1);
    tick(&mut osd);
    osd.host_mut().camera_request = CameraRequest::Off;
    osd.camera_requests();
    assert_eq!(screen(&osd), SETTINGS);
}

#[test]
fn entering_the_camera_view_holds_the_plane_until_the_picture_is_up() {
    let mut osd = osd(with_camera());
    goto(&mut osd, VIDEO);
    osd.ui.global::<Bridge<'_>>().set_screen(CAMERA);
    let mut sink = Sink::default();
    assert!(!osd.tick(false, &mut sink)); // drawn, not pushed
    assert!(sink.0.is_empty());
    assert!(osd.st.held);
    osd.release_held();
    assert!(osd.tick(false, &mut sink));
    assert_eq!(sink.0.len(), 1);
    // Without a release, the hold ends on its own.
    goto(&mut osd, VIDEO);
    osd.ui.global::<Bridge<'_>>().set_screen(CAMERA);
    assert!(!osd.tick(false, &mut sink));
    advance(&mut osd, HOLD_MAX_MS);
    assert!(osd.tick(false, &mut sink));
}

#[test]
fn a_camera_that_fails_goes_back_to_video_saying_why() {
    let mut osd = osd(with_camera());
    goto(&mut osd, VIDEO);
    goto(&mut osd, CAMERA);
    osd.host_mut().camera_failure = Some("No signal on /dev/video1".into());
    tick(&mut osd);
    assert_eq!(screen(&osd), VIDEO);
    assert_eq!(
        osd.ui.global::<Bridge<'_>>().get_camera_error().as_str(),
        "No signal on /dev/video1"
    );
}

// ── bluetooth ────────────────────────────────────────────────────────────

fn device(id: &str, name: &str) -> BtDevice {
    BtDevice {
        id: id.into(),
        name: name.into(),
        detail: String::new(),
        connected: false,
    }
}

#[test]
fn device_rows_are_synced_in_place_by_id() {
    let mut host = TestHost::new();
    host.bt_paired = vec![device("/a", "Keyboard"), device("/b", "Remote")];
    host.bt_found = vec![device("/c", "Zo\u{EB} \u{1F4F1}")];
    let mut osd = osd(host);
    let paired = osd.st.bt_paired.clone();
    let found = osd.st.bt_found.clone();
    assert_eq!(paired.row_count(), 2);
    assert_eq!(
        found.row_data(0).map(|r| r.name.to_string()),
        Some("Zoe".into())
    );
    let host = osd.host_mut();
    host.bt_paired[1].detail = "Connected".into();
    host.bt_paired[1].connected = true;
    host.bt_gen += 1;
    tick(&mut osd);
    let row = paired.row_data(1).expect("row 1");
    assert_eq!(row.detail.as_str(), "Connected");
    assert!(row.connected);
    assert!(Rc::ptr_eq(&paired, &osd.st.bt_paired));

    // The ring on a device that goes away.
    goto(&mut osd, BLUETOOTH);
    for _ in 0..3 {
        press(&mut osd, OsdKey::Down);
    }
    assert_eq!(ring(&osd), "paired:/a");
    let host = osd.host_mut();
    host.bt_paired.remove(0);
    host.bt_gen += 1;
    tick(&mut osd);
    assert_eq!(paired.row_count(), 1);
    assert_eq!(
        paired.row_data(0).map(|r| r.id.to_string()),
        Some("/b".into())
    );
    assert_eq!(ring(&osd), "");
    tick(&mut osd);
    assert!(!osd.st.ring_lost);
}

#[test]
fn the_device_screen_follows_its_device() {
    let mut host = TestHost::new();
    host.bt_paired = vec![device("/a", "Keyboard")];
    let mut osd = osd(host);
    goto(&mut osd, BLUETOOTH);
    assert!(osd.host().has(&Call::BtScreenVisible(true)));
    osd.ui.global::<Bridge<'_>>().invoke_bt_open("/a".into());
    goto(&mut osd, BT_DEVICE);
    assert_eq!(
        osd.ui.global::<Bridge<'_>>().get_bt_current().name.as_str(),
        "Keyboard"
    );
    osd.ui
        .global::<Bridge<'_>>()
        .invoke_bt_connect("/a".into(), true);
    tick(&mut osd);
    assert!(osd.host().has(&Call::BtConnect("/a".into(), true)));
    osd.ui.global::<Bridge<'_>>().invoke_bt_forget("/a".into());
    tick(&mut osd);
    assert!(osd.host().has(&Call::BtForget("/a".into())));
    assert_eq!(screen(&osd), BLUETOOTH);
    osd.ui.global::<Bridge<'_>>().invoke_bt_scan(true);
    tick(&mut osd);
    assert!(osd.host().has(&Call::BtScan(true)));
    osd.hidden();
    assert!(osd.host().has(&Call::BtScreenVisible(false)));
    goto(&mut osd, 3);
    assert_eq!(osd.host().count(&Call::BtScreenVisible(false)), 2);
}

#[test]
fn hidden_drops_the_queued_keys() {
    let mut osd = osd(TestHost::new());
    goto(&mut osd, SETTINGS);
    osd.key(OsdKey::Home, None, 1);
    osd.hidden();
    tick(&mut osd);
    assert_eq!(screen(&osd), SETTINGS);
}

// ── devices ──────────────────────────────────────────────────────────────

fn phone(id: &str, name: &str, detail: &str, active: bool) -> KnownDevice {
    KnownDevice {
        id: id.into(),
        name: name.into(),
        detail: detail.into(),
        active,
        nearby: detail.starts_with("Nearby"),
    }
}

/// Three phones, in the host's order: the connected one, one nearby, one
/// away -- the last never said its name.
fn with_phones() -> TestHost {
    let mut host = TestHost::new();
    host.devices = vec![
        phone("a", "Alex\u{2019}s iPhone", "Connected - CarPlay", true),
        phone("b", "Pixel", "Nearby - Android Auto", false),
        phone("c", "", "Not here - CarPlay", false),
    ];
    host
}

fn device_names(osd: &Osd<TestHost>) -> Vec<String> {
    let rows = &osd.st.devices;
    (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .map(|r| r.name.to_string())
        .collect()
}

fn device_pick(osd: &Osd<TestHost>) -> String {
    osd.ui.global::<Bridge<'_>>().get_device_pick().to_string()
}

#[test]
fn the_devices_screen_lists_the_phones_in_the_hosts_order_and_a_knob_reaches_each() {
    let mut osd = osd(with_phones());
    assert_eq!(device_names(&osd), ["Alex's iPhone", "Pixel", "Phone"]);
    let rows = osd.st.devices.clone();
    assert_eq!(
        rows.row_data(1).map(|r| r.detail.to_string()),
        Some("Nearby - Android Auto".into())
    );
    assert_eq!(rows.row_data(0).map(|r| r.active), Some(true));
    assert_eq!(rows.row_data(1).map(|r| r.nearby), Some(true));
    assert_eq!(rows.row_data(2).map(|r| r.nearby), Some(false));
    // The tile and the Settings row: how many, and the one in session.
    let b = osd.ui.global::<Bridge<'_>>();
    assert_eq!(b.get_phones_summary().as_str(), "3 known");
    assert_eq!(b.get_active_phone().as_str(), "Alex's iPhone");
    // Settings > Phones by the knob: Back, Volume, Night, Phones.
    goto(&mut osd, SETTINGS);
    for _ in 0..4 {
        press(&mut osd, OsdKey::Next);
    }
    press(&mut osd, OsdKey::Ok);
    assert_eq!(screen(&osd), DEVICES);
    // Back on top, then the phones; the ends wrap.
    assert_eq!(ring(&osd), "back");
    press(&mut osd, OsdKey::Next);
    assert_eq!(ring(&osd), "device:a");
    press(&mut osd, OsdKey::Next);
    assert_eq!(ring(&osd), "device:b");
    press(&mut osd, OsdKey::Next);
    assert_eq!(ring(&osd), "device:c");
    press(&mut osd, OsdKey::Next);
    assert_eq!(ring(&osd), "back");
    press(&mut osd, OsdKey::Prev);
    assert_eq!(ring(&osd), "device:c");
    // A press opens the choice on Connect; a turn walks Forget, Cancel and
    // stops; Back closes it.
    press(&mut osd, OsdKey::Ok);
    assert_eq!(pick(&osd), (6, 0));
    assert_eq!(device_pick(&osd), "c");
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Next);
    assert_eq!(pick(&osd), (6, 2));
    press(&mut osd, OsdKey::Back);
    assert_eq!(pick(&osd).0, 0);
    assert_eq!(device_pick(&osd), "");
    assert_eq!(screen(&osd), DEVICES);
    // Open, Connect: the loop is asked to wake the phone.
    press(&mut osd, OsdKey::Ok);
    press(&mut osd, OsdKey::Ok);
    assert!(osd.host().has(&Call::DeviceConnect("c".into())));
    assert_eq!(pick(&osd).0, 0);
    // Cancel asks for nothing.
    press(&mut osd, OsdKey::Ok);
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Ok);
    assert_eq!(osd.host().count(&Call::DeviceConnect("c".into())), 1);
    assert!(!osd.host().has(&Call::DeviceForget("c".into())));
    // Back from the screen: Settings.
    press(&mut osd, OsdKey::Back);
    assert_eq!(screen(&osd), SETTINGS);
}

#[test]
fn forget_asks_first_and_the_connected_phone_offers_stop() {
    let mut osd = osd(with_phones());
    goto(&mut osd, DEVICES);
    press(&mut osd, OsdKey::Down);
    press(&mut osd, OsdKey::Down);
    assert_eq!(ring(&osd), "device:a");
    // The connected phone's row: Stop, Forget, Cancel -- opening on Cancel,
    // and a turn past the end stays there.
    press(&mut osd, OsdKey::Ok);
    assert_eq!(pick(&osd), (6, 2));
    press(&mut osd, OsdKey::Right);
    assert_eq!(pick(&osd), (6, 2));
    press(&mut osd, OsdKey::Left);
    press(&mut osd, OsdKey::Left);
    assert_eq!(pick(&osd), (6, 0));
    // Stop: at once, no question (nothing is lost; Connect brings it back).
    press(&mut osd, OsdKey::Ok);
    assert!(osd.host().has(&Call::DeviceStop("a".into())));
    assert!(!osd.host().has(&Call::DeviceConnect("a".into())));
    assert_eq!(pick(&osd).0, 0);
    press(&mut osd, OsdKey::Ok);
    press(&mut osd, OsdKey::Left);
    assert_eq!(pick(&osd), (6, 1));
    press(&mut osd, OsdKey::Ok); // Forget: asked first
    assert_eq!(pick(&osd).0, 0);
    assert_eq!(prompt_kind(&osd), 2);
    assert_eq!(
        osd.ui.global::<Bridge<'_>>().get_prompt_title().as_str(),
        "Forget Alex's iPhone?"
    );
    assert_eq!(
        osd.ui
            .global::<Bridge<'_>>()
            .get_prompt_accept_label()
            .as_str(),
        "Forget"
    );
    // As for Power: the ring starts on Cancel, and Forget is red.
    assert_eq!(prompt_focus(&osd), 1);
    assert!(osd.ui.global::<Bridge<'_>>().get_prompt_danger());
    press(&mut osd, OsdKey::Ok);
    assert_eq!(prompt_kind(&osd), 0);
    assert!(!osd.host().has(&Call::DeviceForget("a".into())));
    assert_eq!(device_names(&osd).len(), 3);
    // The dialog closed: the screen is made again, the ring back on the
    // phone's row.
    assert_eq!(ring(&osd), "device:a");
    // Asked again and answered: the phone goes.
    press(&mut osd, OsdKey::Ok);
    press(&mut osd, OsdKey::Left);
    assert_eq!(pick(&osd), (6, 1));
    press(&mut osd, OsdKey::Ok);
    press(&mut osd, OsdKey::Right); // from Cancel to Forget
    assert_eq!(prompt_focus(&osd), 0);
    press(&mut osd, OsdKey::Ok);
    assert!(osd.host().has(&Call::DeviceForget("a".into())));
    assert_eq!(prompt_kind(&osd), 0);
    assert_eq!(device_names(&osd), ["Pixel", "Phone"]);
    // Its row is gone: the ring goes to the first stop.
    assert_eq!(ring(&osd), "back");
    press(&mut osd, OsdKey::Down);
    assert_eq!(ring(&osd), "device:b");
}

#[test]
fn a_phone_is_picked_by_touch_too() {
    let mut osd = osd(with_phones());
    goto(&mut osd, SETTINGS);
    goto(&mut osd, DEVICES);
    // The bar, then the phones from y 64, 144 and 224; the options of a
    // choosing row end 12 px before its right edge: 120 px each, 8 px
    // apart.
    let tap = |osd: &mut Osd<TestHost>, x: i32, y: i32| {
        osd.pointer(x, y, true);
        osd.pointer(x, y, false);
        tick(osd);
    };
    tap(&mut osd, 300, 180); // Pixel
    assert_eq!(pick(&osd), (6, 0));
    assert_eq!(device_pick(&osd), "b");
    assert_eq!(ring(&osd), ""); // touch never shows a ring
    tap(&mut osd, 720, 180); // Cancel
    assert_eq!(pick(&osd).0, 0);
    assert!(
        !osd.host()
            .calls
            .iter()
            .any(|c| matches!(c, Call::DeviceConnect(_) | Call::DeviceForget(_)))
    );
    tap(&mut osd, 300, 180);
    tap(&mut osd, 464, 180); // Connect
    assert!(osd.host().has(&Call::DeviceConnect("b".into())));
    assert_eq!(pick(&osd).0, 0);
    tap(&mut osd, 300, 180);
    tap(&mut osd, 592, 180); // Forget: asked
    assert_eq!(prompt_kind(&osd), 2);
    assert_eq!(
        osd.ui.global::<Bridge<'_>>().get_prompt_title().as_str(),
        "Forget Pixel?"
    );
    osd.ui.global::<Bridge<'_>>().invoke_prompt_accept();
    tick(&mut osd);
    assert!(osd.host().has(&Call::DeviceForget("b".into())));
    assert_eq!(device_names(&osd), ["Alex's iPhone", "Phone"]);
    // The connected phone's row: Stop, Forget, Cancel.
    tap(&mut osd, 300, 100);
    assert_eq!(pick(&osd), (6, 2));
    tap(&mut osd, 592, 100); // Forget, in the middle
    assert_eq!(prompt_kind(&osd), 2);
    assert_eq!(
        osd.ui.global::<Bridge<'_>>().get_prompt_title().as_str(),
        "Forget Alex's iPhone?"
    );
    osd.ui.global::<Bridge<'_>>().invoke_prompt_cancel();
    tick(&mut osd);
    assert!(!osd.host().has(&Call::DeviceForget("a".into())));
    // Back, the bar's key.
    tap(&mut osd, 32, 28);
    assert_eq!(screen(&osd), SETTINGS);
}

#[test]
fn an_empty_registry_shows_only_back() {
    let mut osd = osd(TestHost::new());
    assert_eq!(
        osd.ui.global::<Bridge<'_>>().get_phones_summary().as_str(),
        "None yet"
    );
    // Opened from Home (its Phones tile), Back goes Home.
    goto(&mut osd, DEVICES);
    assert_eq!(osd.st.devices.row_count(), 0);
    // The ring has nowhere to go but Back (the "No phones yet" line is no
    // row), and Enter leaves.
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Next);
    assert_eq!(ring(&osd), "back");
    press(&mut osd, OsdKey::Ok);
    assert_eq!(screen(&osd), HOME);
    // A phone arriving is listed where the host has it.
    let host = osd.host_mut();
    host.devices
        .push(phone("z", "Zo\u{EB} \u{1F4F1}", "Nearby - CarPlay", false));
    host.devices_gen += 1;
    tick(&mut osd);
    assert_eq!(device_names(&osd), ["Zoe"]);
}

#[test]
fn device_rows_follow_the_hosts_order_in_place_and_a_change_ends_the_choice() {
    let mut osd = osd(with_phones());
    let rows = osd.st.devices.clone();
    goto(&mut osd, DEVICES);
    // An unchanged generation costs no sync; a changed row is updated in
    // place, in the same model.
    let host = osd.host_mut();
    host.devices[1].detail = "Not here - Android Auto".into();
    tick(&mut osd);
    assert_eq!(
        rows.row_data(1).map(|r| r.detail.to_string()),
        Some("Nearby - Android Auto".into())
    );
    osd.host_mut().devices_gen += 1;
    tick(&mut osd);
    assert_eq!(
        rows.row_data(1).map(|r| r.detail.to_string()),
        Some("Not here - Android Auto".into())
    );
    assert!(Rc::ptr_eq(&rows, &osd.st.devices));
    // The ring on the last phone, its choice open: the phone connects and
    // goes to the top -- the choice ends, the ring is put back on Back.
    for _ in 0..4 {
        press(&mut osd, OsdKey::Down);
    }
    assert_eq!(ring(&osd), "device:c");
    press(&mut osd, OsdKey::Ok);
    assert_eq!(pick(&osd), (6, 0));
    let host = osd.host_mut();
    let mut c = host.devices.remove(2);
    c.active = true;
    c.detail = "Connected - CarPlay".into();
    host.devices[0].active = false;
    host.devices[0].detail = "Nearby - CarPlay".into();
    host.devices.insert(0, c);
    host.devices_gen += 1;
    tick(&mut osd);
    assert_eq!(device_names(&osd), ["Phone", "Alex's iPhone", "Pixel"]);
    assert_eq!(rows.row_data(0).map(|r| r.active), Some(true));
    assert_eq!(pick(&osd).0, 0);
    assert_eq!(device_pick(&osd), "");
    assert_eq!(ring(&osd), "");
    tick(&mut osd);
    assert_eq!(ring(&osd), "back");
    press(&mut osd, OsdKey::Down);
    assert_eq!(ring(&osd), "device:c");
    // Left alone, a choice ends on its own; off its screen too.
    press(&mut osd, OsdKey::Ok);
    advance(&mut osd, PICK_MS);
    tick(&mut osd);
    assert_eq!(pick(&osd).0, 0);
    press(&mut osd, OsdKey::Ok);
    assert_eq!(pick(&osd).0, 6);
    press(&mut osd, OsdKey::Home);
    assert_eq!(screen(&osd), HOME);
    assert_eq!(pick(&osd).0, 0);
}

// ── now playing and the navigation card ──────────────────────────────────

fn playing(osd: &mut Osd<TestHost>) {
    let now = osd.st.now();
    let host = osd.host_mut();
    host.media = crate::host::Media {
        available: true,
        title: "A Very Long Song Title That Goes On \u{2013} And On".into(),
        artist: "Zo\u{eb} Artist".into(),
        album: "The Album".into(),
        duration_ms: 220_000,
        position_ms: 134_400,
        position_at: now,
        playing: true,
    };
    host.media_gen += 1;
    tick(osd);
}

fn media_text(osd: &Osd<TestHost>) -> (String, String, String, String) {
    let b = osd.ui.global::<Bridge<'_>>();
    (
        b.get_media_title().to_string(),
        b.get_media_artist().to_string(),
        b.get_media_album().to_string(),
        format!("{} / {}", b.get_media_elapsed(), b.get_media_total()),
    )
}

/// The progress bar, in thousandths; -1 none.
fn progress(osd: &Osd<TestHost>) -> i32 {
    (osd.ui.global::<Bridge<'_>>().get_media_progress() * 1000.0).round() as i32
}

#[test]
fn the_home_tile_opens_the_media_page_and_the_knob_reaches_its_keys() {
    let mut osd = osd(TestHost::new());
    assert!(!osd.ui.global::<Bridge<'_>>().get_media_available());
    playing(&mut osd);
    let b = osd.ui.global::<Bridge<'_>>();
    assert!(b.get_media_available());
    // The tile's bar: 134 s of 220, on Home in steps of five seconds.
    assert_eq!(progress(&osd), 590);
    // The knob: Connect, then Now playing; its press opens the page.
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Next);
    assert_eq!(ring(&osd), "media");
    press(&mut osd, OsdKey::Ok);
    assert_eq!(screen(&osd), MEDIA);
    assert_eq!(
        media_text(&osd),
        (
            "A Very Long Song Title That Goes On - And On".into(),
            "Zoe Artist".into(),
            "The Album".into(),
            "2:14 / 3:40".into()
        )
    );
    assert_eq!(progress(&osd), 609);
    // Back has the ring; then Previous, Play/Pause, Next.
    assert_eq!(ring(&osd), "back");
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Ok);
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Ok);
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Ok);
    use crate::host::MediaKey;
    let keys: Vec<_> = osd
        .host()
        .calls
        .iter()
        .filter_map(|c| match c {
            Call::MediaKey(k) => Some(*k),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        [MediaKey::Previous, MediaKey::PlayPause, MediaKey::Next]
    );
    // Back from the page is Home, the ring on its tile.
    press(&mut osd, OsdKey::Back);
    assert_eq!(screen(&osd), HOME);
    assert_eq!(ring(&osd), "media");
}

#[test]
fn the_media_page_by_touch_and_its_running_position() {
    let mut osd = osd(TestHost::new());
    playing(&mut osd);
    // The Now playing tile: top right, beside Connect.
    osd.pointer(600, 160, true);
    osd.pointer(600, 160, false);
    tick(&mut osd);
    assert_eq!(screen(&osd), MEDIA);
    assert_eq!(media_text(&osd).3, "2:14 / 3:40");
    // The line moves on once a second, and the loop is told when.
    let due = osd.next_due().expect("a second to come");
    assert!(due.as_millis() <= 600, "{due:?}");
    advance(&mut osd, 700);
    tick(&mut osd);
    assert_eq!(media_text(&osd).3, "2:15 / 3:40");
    tick(&mut osd);
    advance(&mut osd, 500);
    let mut sink = Sink::default();
    assert!(!osd.tick(false, &mut sink), "the same second draws nothing");
    // The three keys share the rest of the page under the text.
    for (x, key) in [
        (136, crate::host::MediaKey::Previous),
        (400, crate::host::MediaKey::PlayPause),
        (664, crate::host::MediaKey::Next),
    ] {
        osd.pointer(x, 350, true);
        osd.pointer(x, 350, false);
        tick(&mut osd);
        assert_eq!(osd.host().calls.last(), Some(&Call::MediaKey(key)));
    }
    // Paused: Play is offered and the position stands.
    let host = osd.host_mut();
    host.media.playing = false;
    host.media_gen += 1;
    tick(&mut osd);
    assert!(!osd.ui.global::<Bridge<'_>>().get_media_playing());
    advance(&mut osd, 10_000);
    tick(&mut osd);
    assert_eq!(media_text(&osd).3, "2:14 / 3:40");
    assert_eq!(osd.next_due(), None);
    // The session went: the page stays, saying so; the row on Home goes.
    let host = osd.host_mut();
    host.media = crate::host::Media::default();
    host.media_gen += 1;
    tick(&mut osd);
    assert_eq!(
        media_text(&osd),
        (String::new(), String::new(), String::new(), " / ".into())
    );
    assert_eq!(progress(&osd), -1000);
    assert!(!osd.ui.global::<Bridge<'_>>().get_media_available());
}

#[test]
fn the_album_art_is_taken_once_and_shown_beside_the_words() {
    use crate::host::Art;
    let mut osd = osd(TestHost::new());
    playing(&mut osd);
    goto(&mut osd, MEDIA);
    assert!(!osd.ui.global::<Bridge<'_>>().get_media_art_shown());
    let rgb: Vec<u8> = (0..160 * 120 * 3).map(|i| (i % 251) as u8).collect();
    osd.host_mut().art = Some(Art::from_rgb(160, 120, &rgb));
    tick(&mut osd);
    let b = osd.ui.global::<Bridge<'_>>();
    assert!(b.get_media_art_shown());
    let size = b.get_media_art().size();
    assert_eq!((size.width, size.height), (160, 120));
    assert!(osd.host().art.is_none(), "taken, not asked again");
    // The position line moves on; the art stays as it was set.
    advance(&mut osd, 1000);
    tick(&mut osd);
    assert_eq!(media_text(&osd).3, "2:15 / 3:40");
    assert!(osd.ui.global::<Bridge<'_>>().get_media_art_shown());
    // No art: the words take the width again.
    osd.host_mut().art = Some(Art::default());
    tick(&mut osd);
    let b = osd.ui.global::<Bridge<'_>>();
    assert!(!b.get_media_art_shown());
    let size = b.get_media_art().size();
    assert_eq!((size.width, size.height), (0, 0));
    // Pixels that do not fill the size are no art.
    assert!(Art::from_rgb(2, 2, &[0; 3]).is_none());
    assert!(Art::from_rgb(0, 0, &[]).is_none());
    assert_eq!(Art::from_rgb(1, 1, &[1, 2, 3]).size(), (1, 1));
}

#[test]
fn the_now_playing_tile_going_puts_the_ring_back() {
    let mut osd = osd(TestHost::new());
    playing(&mut osd);
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Next);
    assert_eq!(ring(&osd), "media");
    let host = osd.host_mut();
    host.media.available = false;
    host.media_gen += 1;
    tick(&mut osd);
    tick(&mut osd);
    assert_eq!(ring(&osd), "connect");
    press(&mut osd, OsdKey::Ok); // Connect a phone: the Phones screen
    assert_eq!(screen(&osd), DEVICES);
}

#[test]
fn positions_read_as_a_player_shows_them() {
    let mut m = crate::host::Media::default();
    assert_eq!(media::clock_text(59), "0:59");
    assert_eq!(media::clock_text(3725), "1:02:05");
    assert_eq!(media::clock_text(3600), "1:00:00");
    m.available = true;
    m.playing = true;
    m.position_ms = 1000;
    m.position_at = 100;
    m.duration_ms = 1500;
    assert_eq!(media::position(&m, 300), 1200);
    assert_eq!(media::position(&m, 9000), 1500);
}

fn guiding(osd: &mut Osd<TestHost>) {
    let host = osd.host_mut();
    host.nav = crate::host::NavCard {
        active: true,
        arrow: crate::host::NavArrow::Left,
        maneuver: "Turn left".into(),
        distance: "350 m".into(),
        road: "Stra\u{df}e".into(),
        eta: "14:32".into(),
        eta_note: "arrival".into(),
    };
    host.nav_gen += 1;
}

fn card(osd: &Osd<TestHost>) -> bool {
    osd.ui.global::<Bridge<'_>>().get_nav_card()
}

#[test]
fn the_card_shows_for_thirty_seconds_after_each_update() {
    let mut osd = osd(TestHost::new());
    assert!(!card(&osd));
    guiding(&mut osd);
    let mut sink = Sink::default();
    assert!(osd.tick(false, &mut sink));
    assert!(card(&osd));
    assert_eq!(sink.0, [(0, H)], "the screens move down under it");
    let b = osd.ui.global::<Bridge<'_>>();
    assert_eq!(b.get_nav_maneuver().as_str(), "Turn left");
    assert_eq!(b.get_nav_distance().as_str(), "350 m");
    assert_eq!(b.get_nav_road().as_str(), "Strasse");
    assert_eq!(b.get_nav_eta().as_str(), "14:32");
    assert_eq!(b.get_nav_eta_note().as_str(), "arrival");
    assert_eq!(b.get_nav_arrow(), 3);
    // On the screens it is part of them, no window.
    assert_eq!(osd.over_video(), None);
    // (The clock under the skew runs on too: a margin of 100 ms.)
    advance(&mut osd, NAV_CARD_MS - 100);
    tick(&mut osd);
    assert!(card(&osd));
    let due = osd.next_due().expect("the card's end");
    assert!(due.as_millis() <= 100, "{due:?}");
    advance(&mut osd, 100);
    tick(&mut osd);
    assert!(!card(&osd));
    assert_eq!(osd.next_due(), None);
    // Each update keeps it up for another thirty seconds.
    guiding(&mut osd);
    tick(&mut osd);
    advance(&mut osd, NAV_CARD_MS / 2);
    osd.host_mut().nav_gen += 1;
    tick(&mut osd);
    advance(&mut osd, NAV_CARD_MS - 100);
    tick(&mut osd);
    assert!(card(&osd));
    // Guidance stopping hides it at once.
    osd.host_mut().nav.active = false;
    osd.host_mut().nav_gen += 1;
    tick(&mut osd);
    assert!(!card(&osd));
}

#[test]
fn the_card_never_covers_the_phone_and_is_the_window_over_the_camera() {
    let mut osd = osd(with_camera());
    guiding(&mut osd);
    tick(&mut osd);
    assert!(card(&osd));
    // The phone's video in front: only a window shows, never the card.
    osd.hidden();
    osd.tick(true, &mut Sink::default());
    assert!(!card(&osd));
    assert_eq!(osd.over_video(), None);
    osd.invalidate();
    tick(&mut osd);
    assert!(card(&osd));
    // A dialog takes the whole screen.
    bt_question(&mut osd);
    assert!(!card(&osd));
    osd.host_mut().bt_prompt = Prompt::default();
    osd.host_mut().bt_gen += 1;
    tick(&mut osd);
    assert!(card(&osd));

    // The camera view: the column first, then the card as the window.
    goto(&mut osd, VIDEO);
    goto(&mut osd, CAMERA);
    let over = |osd: &mut Osd<TestHost>| osd.tick(true, &mut Sink::default());
    over(&mut osd);
    assert!(!card(&osd));
    assert_eq!(osd.over_video().map(|r| r.x), Some(620));
    advance(&mut osd, CAMERA_PANEL_MS);
    over(&mut osd);
    assert!(card(&osd));
    let band = Rect {
        x: 0,
        y: 0,
        width: 800,
        height: 88,
    };
    assert_eq!(osd.over_video(), Some(band));
    // A volume change takes the window for its moment, then the card is back.
    osd.host_mut().volume_gen += 1;
    over(&mut osd);
    let bar = osd.over_video().expect("the bar");
    assert_eq!((bar.width, bar.height), (180, 48));
    advance(&mut osd, VOLUME_BAR_MS);
    over(&mut osd);
    assert_eq!(osd.over_video(), Some(band));
    // And it goes with the guidance.
    advance(&mut osd, NAV_CARD_MS);
    over(&mut osd);
    assert_eq!(osd.over_video(), None);
}

// ── Settings > Audio ─────────────────────────────────────────────────────

/// A text property of the Bridge, as a String.
macro_rules! prop {
    ($osd:expr, $get:ident) => {
        $osd.ui.global::<Bridge<'_>>().$get().to_string()
    };
}

fn with_outputs() -> TestHost {
    let mut host = TestHost::new();
    host.outputs = vec![
        AudioOutput {
            device: "hw:0,0".into(),
            label: "sun4i-codec".into(),
            active: false,
        },
        AudioOutput {
            device: "hw:CARD=Device,DEV=0".into(),
            label: "USB Audio Device".into(),
            active: false,
        },
    ];
    host
}

#[test]
fn the_audio_row_opens_the_stream_levels_which_turn_live_in_tens() {
    let mut osd = osd(with_outputs());
    goto(&mut osd, SETTINGS);
    // By the knob: Back, Volume, Night, Phones, Audio.
    for _ in 0..5 {
        press(&mut osd, OsdKey::Next);
    }
    press(&mut osd, OsdKey::Ok);
    assert_eq!(screen(&osd), AUDIO);
    assert_eq!(prop!(osd, get_music_label), "100%");
    assert_eq!(osd.ui.global::<Bridge<'_>>().get_music_level(), 100);
    // Music: a turn down is 90 %, applied (persisted) at once.
    pick_press(&mut osd, 7);
    assert_eq!(pick(&osd).0, 7);
    press(&mut osd, OsdKey::Prev);
    assert_eq!(
        osd.host().persisted("audio.music-level"),
        Some(&toml::Value::Integer(90))
    );
    assert_eq!(prop!(osd, get_music_label), "90%");
    assert_eq!(osd.ui.global::<Bridge<'_>>().get_music_level(), 90);
    // It stops at the top, and Up is more.
    for _ in 0..3 {
        press(&mut osd, OsdKey::Up);
    }
    assert_eq!(osd.host().settings.audio.music_level, 100);
    press(&mut osd, OsdKey::Ok);
    assert_eq!(pick(&osd).0, 0);
    // Calls down to nothing: Off.
    pick_press(&mut osd, 10);
    for _ in 0..12 {
        press(&mut osd, OsdKey::Down);
    }
    assert_eq!(osd.host().settings.audio.call_level, 0);
    assert_eq!(prop!(osd, get_call_label), "Off");
    press(&mut osd, OsdKey::Back);
    assert_eq!(pick(&osd).0, 0);
    // - / + by touch on Navigation.
    pick_press(&mut osd, 8);
    osd.ui.global::<Bridge<'_>>().invoke_pick_step(-1);
    tick(&mut osd);
    assert_eq!(prop!(osd, get_navigation_label), "90%");
    // Off its screen the pick ends.
    goto(&mut osd, SETTINGS);
    assert_eq!(pick(&osd).0, 0);
}

#[test]
fn the_output_steps_through_the_cards_and_is_taken_with_enter() {
    let mut osd = osd(with_outputs());
    // Listed at the first look, for the Settings row.
    assert_eq!(osd.host().count(&Call::AudioOutputs), 1);
    assert_eq!(prop!(osd, get_output_label), "sun4i-codec");
    goto(&mut osd, AUDIO);
    assert_eq!(osd.host().count(&Call::AudioOutputs), 2);
    pick_press(&mut osd, 11);
    assert_eq!(pick(&osd), (11, 0));
    assert_eq!(prop!(osd, get_pick_value), "sun4i-codec");
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Next); // stops at the end
    assert_eq!(pick(&osd), (11, 1));
    assert_eq!(prop!(osd, get_pick_value), "USB Audio Device");
    // Back leaves it as it was.
    press(&mut osd, OsdKey::Back);
    assert!(osd.host().persisted("audio.device").is_none());
    // Enter takes it (the program follows at once), the microphone where
    // it was.
    pick_press(&mut osd, 11);
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Ok);
    assert_eq!(
        osd.host().persisted("audio.device"),
        Some(&toml::Value::String("hw:CARD=Device,DEV=0".into()))
    );
    assert_eq!(
        osd.host().persisted("audio.mic-device"),
        Some(&toml::Value::String("hw:0,0".into()))
    );
    assert_eq!(prop!(osd, get_output_label), "USB Audio Device");
    // Taking the one in use writes nothing.
    let calls = osd.host().calls.len();
    pick_press(&mut osd, 11);
    assert_eq!(pick(&osd), (11, 1));
    press(&mut osd, OsdKey::Ok);
    assert_eq!(osd.host().calls.len(), calls);
    // No cards: nothing to step through.
    osd.host_mut().outputs.clear();
    goto(&mut osd, SETTINGS);
    goto(&mut osd, AUDIO);
    pick_press(&mut osd, 11);
    assert_eq!(pick(&osd).0, 0);
    // The card it names is gone: said so, never the raw PCM name.
    assert_eq!(prop!(osd, get_output_label), "Not found");
}

#[test]
fn the_settings_row_names_the_card_even_before_the_audio_page() {
    let mut host = with_outputs();
    // The setting names the card by number, the list by its id.
    host.outputs[0].device = "hw:CARD=Codec,DEV=0".into();
    let mut screens = osd(host);
    assert_eq!(prop!(screens, get_output_label), "Not found");
    // The program's list matches it by number: Settings looks again.
    screens.host_mut().outputs[0].device = "hw:0,0".into();
    goto(&mut screens, SETTINGS);
    assert_eq!(prop!(screens, get_output_label), "sun4i-codec");
    // No setting at all: the system's default.
    let mut host = with_outputs();
    host.settings.audio.device = String::new();
    host.outputs.clear();
    let fresh = osd(host);
    assert_eq!(prop!(fresh, get_output_label), "Default");
}

// ── the access point ─────────────────────────────────────────────────────

/// The band and width as the options shown taken (-1 none), the channel,
/// the country and the status.
fn ap_labels(osd: &Osd<TestHost>) -> [String; 5] {
    let b = osd.ui.global::<Bridge<'_>>();
    [
        b.get_ap_band_choice().to_string(),
        b.get_ap_width_choice().to_string(),
        prop!(osd, get_ap_channel_label),
        prop!(osd, get_ap_country_label),
        prop!(osd, get_ap_status),
    ]
}

fn radio(band: lehu_wifi::Band, channel: u32, width: u32, country: &str) -> lehu_wifi::Radio {
    lehu_wifi::Radio {
        band,
        channel,
        width,
        country: country.into(),
    }
}

#[test]
fn the_access_point_page_picks_the_radio_and_says_how_it_went() {
    use lehu_wifi::Band;
    let mut osd = osd(TestHost::new());
    goto(&mut osd, 3);
    // By the knob: Back, Access point.
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Ok);
    assert_eq!(screen(&osd), ACCESS_POINT);
    assert_eq!(ap_labels(&osd), ["0", "0", "6", "Not set", ""]);
    assert!(!osd.ui.global::<Bridge<'_>>().get_ap_five());
    // 5 GHz without a country: said, nothing written.
    pick_press(&mut osd, 12);
    assert_eq!(pick(&osd), (12, 0));
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Ok);
    tick(&mut osd);
    assert_eq!(ap_labels(&osd)[4], "5 GHz needs the country");
    assert!(
        !osd.host()
            .calls
            .iter()
            .any(|c| matches!(c, Call::ConfigureRadio(_)))
    );
    // The country, stepped to DE.
    pick_press(&mut osd, 15);
    assert_eq!(prop!(osd, get_pick_value), "AT");
    for _ in 0..10 {
        press(&mut osd, OsdKey::Next);
    }
    assert_eq!(prop!(osd, get_pick_value), "DE");
    press(&mut osd, OsdKey::Ok);
    // "Restarting..." is drawn first; the write is the next tick's.
    assert_eq!(ap_labels(&osd)[4], "Restarting...");
    assert!(
        !osd.host()
            .calls
            .iter()
            .any(|c| matches!(c, Call::ConfigureRadio(_)))
    );
    tick(&mut osd);
    assert!(
        osd.host()
            .has(&Call::ConfigureRadio(radio(Band::G2, 6, 20, "DE")))
    );
    assert_eq!(ap_labels(&osd), ["0", "0", "6", "DE", "Restarted"]);
    // Now 5 GHz: the channel moves to 36 with it.
    pick_press(&mut osd, 12);
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Ok);
    tick(&mut osd);
    assert!(
        osd.host()
            .has(&Call::ConfigureRadio(radio(Band::G5, 36, 20, "DE")))
    );
    assert!(osd.ui.global::<Bridge<'_>>().get_ap_five());
    // 80 MHz by a tap on its option.
    pick_press(&mut osd, 13);
    osd.ui.global::<Bridge<'_>>().invoke_pick_option(2);
    tick(&mut osd);
    tick(&mut osd);
    assert_eq!(ap_labels(&osd)[1], "2");
    // Only the channels allowed: DE has 36 to 48.
    pick_press(&mut osd, 14);
    assert_eq!(prop!(osd, get_pick_value), "36");
    for _ in 0..9 {
        press(&mut osd, OsdKey::Up);
    }
    assert_eq!(prop!(osd, get_pick_value), "48");
    press(&mut osd, OsdKey::Ok);
    tick(&mut osd);
    assert_eq!(osd.host().ap, Some(radio(Band::G5, 48, 80, "DE")));
    assert_eq!(ap_labels(&osd)[2], "48");
    // Not allowed to write: said, the page as it was.
    osd.host_mut().ap_denied = true;
    pick_press(&mut osd, 14);
    press(&mut osd, OsdKey::Prev);
    press(&mut osd, OsdKey::Ok);
    tick(&mut osd);
    assert_eq!(ap_labels(&osd)[4], "Not allowed to change it");
    assert_eq!(ap_labels(&osd)[2], "48");
    // The name too, from the editor over the page.
    edit(&mut osd, 0);
    typed(&mut osd, '2');
    press(&mut osd, OsdKey::Ok);
    tick(&mut osd);
    assert_eq!(osd.st.editing, -1);
    assert!(osd.host().has(&Call::ConfigureAccessPoint(
        "LEHU2".into(),
        "lehu1234".into()
    )));
    assert_eq!(ap_labels(&osd)[4], "Not allowed to change it");
    // Saved but not restarted.
    let host = osd.host_mut();
    host.ap_denied = false;
    host.ap_applied = lehu_wifi::Applied::AtBoot;
    pick_press(&mut osd, 14);
    press(&mut osd, OsdKey::Prev);
    press(&mut osd, OsdKey::Ok);
    tick(&mut osd);
    assert_eq!(ap_labels(&osd)[4], "Saved, applies at the next boot");
    // Left and opened again: the status goes.
    press(&mut osd, OsdKey::Back);
    assert_eq!(screen(&osd), 3);
    goto(&mut osd, ACCESS_POINT);
    assert_eq!(ap_labels(&osd)[4], "");
}

#[test]
fn an_access_point_without_its_config_says_so_and_picks_nothing() {
    let mut host = TestHost::new();
    host.ap = None;
    let mut osd = osd(host);
    goto(&mut osd, ACCESS_POINT);
    assert_eq!(
        ap_labels(&osd),
        ["-1", "-1", "", "", "No access point config"]
    );
    for row in 12..=15 {
        pick_press(&mut osd, row);
        assert_eq!(pick(&osd).0, 0);
    }
}

// ── the link speed page ──────────────────────────────────────────────────

fn link_labels(osd: &Osd<TestHost>) -> [String; 6] {
    [
        prop!(osd, get_link_radio),
        prop!(osd, get_link_phone),
        prop!(osd, get_link_signal),
        prop!(osd, get_link_rate),
        prop!(osd, get_link_receiving),
        prop!(osd, get_link_sending),
    ]
}

#[test]
fn the_link_page_samples_once_a_second_only_while_up() {
    let mut host = TestHost::new();
    host.link = lehu_wifi::Link {
        up: true,
        channel: Some(lehu_wifi::Channel {
            mhz: 5180,
            width_mhz: 80,
        }),
        station: Some(lehu_wifi::Station {
            signal_dbm: -58,
            tx_kbps: 433_300,
            rx_kbps: 390_000,
            ..Default::default()
        }),
        stations: 1,
        tx_bytes: 1000,
        rx_bytes: 1000,
    };
    let mut osd = osd(host);
    goto(&mut osd, 3);
    // By the knob: Back, Access point, Link speed.
    for _ in 0..3 {
        press(&mut osd, OsdKey::Next);
    }
    press(&mut osd, OsdKey::Ok);
    assert_eq!(screen(&osd), LINK);
    assert_eq!(osd.host().count(&Call::Link), 1);
    assert_eq!(
        link_labels(&osd),
        [
            "5 GHz, channel 36, 80 MHz",
            "Connected, no session",
            "-58 dBm, good",
            "390 Mbit/s",
            "Measuring...",
            "Measuring...",
        ]
    );
    // Not before the second is up; the loop is told when.
    tick(&mut osd);
    assert_eq!(osd.host().count(&Call::Link), 1);
    assert!(osd.next_due().is_some_and(|d| d.as_millis() <= 1000));
    advance(&mut osd, 1000);
    let host = osd.host_mut();
    host.link.tx_bytes += 1_000_000;
    host.link.rx_bytes += 50_000;
    tick(&mut osd);
    assert_eq!(osd.host().count(&Call::Link), 2);
    // 8 Mbit over the second, give or take the clock's own steps.
    let [.., receiving, sending] = link_labels(&osd);
    let to: f32 = sending.strip_suffix(" Mbit/s").unwrap().parse().unwrap();
    assert!((7.5..=8.0).contains(&to), "{sending}");
    assert!(
        receiving == "0.4 Mbit/s" || receiving == "0.3 Mbit/s",
        "{receiving}"
    );
    // A wireless session names its phone.
    let host = osd.host_mut();
    host.link_wifi = true;
    host.status = "CarPlay: Alex's iPhone".into();
    advance(&mut osd, 1000);
    tick(&mut osd);
    assert_eq!(link_labels(&osd)[1], "CarPlay: Alex's iPhone");
    // A driver that reports no station (this board's): no signal, no
    // speed, and those lines go; the phone gone, the AP down.
    let host = osd.host_mut();
    host.link_wifi = false;
    host.link.station = None;
    host.link.up = false;
    advance(&mut osd, 1000);
    tick(&mut osd);
    assert_eq!(link_labels(&osd)[..4], ["Not running", "None", "", ""]);
    // Off the page: no more samples, no more wake-ups for it.
    press(&mut osd, OsdKey::Back);
    assert_eq!(screen(&osd), 3);
    advance(&mut osd, 5000);
    tick(&mut osd);
    assert_eq!(osd.host().count(&Call::Link), 4);
    assert!(osd.next_due().is_none());
}

// ── the redesign's behaviours ────────────────────────────────────────────

#[test]
fn back_puts_the_ring_on_the_row_that_opened_the_page() {
    let mut host = TestHost::new();
    host.bt_paired = vec![device("/k", "Remote")];
    let mut osd = osd(host);
    // Home: Connect, Phones, Settings -- Settings opens.
    for _ in 0..3 {
        press(&mut osd, OsdKey::Next);
    }
    assert_eq!(ring(&osd), "settings");
    press(&mut osd, OsdKey::Ok);
    assert_eq!(screen(&osd), SETTINGS);
    // A page opened forward starts on its back key.
    assert_eq!(ring(&osd), "back");
    // Settings: Back, Volume, Night, Phones, Audio, Wireless.
    for _ in 0..5 {
        press(&mut osd, OsdKey::Next);
    }
    assert_eq!(ring(&osd), "wireless");
    press(&mut osd, OsdKey::Ok);
    // Wireless: Back, Access point, Link speed, Bluetooth name, Bluetooth.
    for _ in 0..4 {
        press(&mut osd, OsdKey::Next);
    }
    assert_eq!(ring(&osd), "bluetooth");
    press(&mut osd, OsdKey::Ok);
    assert_eq!(screen(&osd), BLUETOOTH);
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Next);
    assert_eq!(ring(&osd), "paired:/k");
    press(&mut osd, OsdKey::Ok);
    assert_eq!(screen(&osd), BT_DEVICE);
    // Back, step by step: each page with the ring where it was left.
    press(&mut osd, OsdKey::Back);
    assert_eq!((screen(&osd), ring(&osd)), (BLUETOOTH, "paired:/k".into()));
    press(&mut osd, OsdKey::Back);
    assert_eq!((screen(&osd), ring(&osd)), (WIRELESS, "bluetooth".into()));
    // The bar's back key does the same.
    osd.ui.global::<Bridge<'_>>().invoke_back();
    tick(&mut osd);
    assert_eq!((screen(&osd), ring(&osd)), (SETTINGS, "wireless".into()));
    // Home from deep down: the tile that opened Settings.
    press(&mut osd, OsdKey::Ok);
    press(&mut osd, OsdKey::Home);
    assert_eq!((screen(&osd), ring(&osd)), (HOME, "settings".into()));
    // A row that is gone: the first stop instead.
    goto(&mut osd, WIRELESS);
    for _ in 0..4 {
        press(&mut osd, OsdKey::Next);
    }
    assert_eq!(ring(&osd), "bluetooth");
    press(&mut osd, OsdKey::Ok);
    assert_eq!(screen(&osd), BLUETOOTH);
    osd.host_mut().bt_available = false;
    osd.host_mut().bt_gen += 1;
    tick(&mut osd);
    press(&mut osd, OsdKey::Back);
    assert_eq!((screen(&osd), ring(&osd)), (WIRELESS, "back".into()));
    // Touch never shows a ring: nothing is put back for it.
    advance(&mut osd, KEY_RECENT_MS);
    goto(&mut osd, 3);
    osd.ui.global::<Bridge<'_>>().invoke_back();
    tick(&mut osd);
    assert_eq!((screen(&osd), ring(&osd)), (SETTINGS, "".into()));
}

#[test]
fn a_dialog_closing_puts_the_ring_back_on_its_row() {
    let mut osd = osd(TestHost::new());
    goto(&mut osd, SETTINGS);
    // Back, Volume, Night, Phones, Audio, Wireless, Video, Power.
    for _ in 0..8 {
        press(&mut osd, OsdKey::Next);
    }
    assert_eq!(ring(&osd), "power");
    press(&mut osd, OsdKey::Ok);
    press(&mut osd, OsdKey::Left); // Power off
    press(&mut osd, OsdKey::Ok);
    assert_eq!(prompt_kind(&osd), 2);
    press(&mut osd, OsdKey::Ok); // Cancel, ringed from the start
    assert_eq!(prompt_kind(&osd), 0);
    assert_eq!(ring(&osd), "power");
}

#[test]
fn a_level_turned_fast_moves_further_a_detent() {
    let mut osd = osd(with_outputs());
    goto(&mut osd, SETTINGS);
    pick_press(&mut osd, 4);
    let volume = |osd: &Osd<TestHost>| osd.host().volume_level;
    assert_eq!(volume(&osd), 60);
    // Slow detents: one step (5 %) each.
    press(&mut osd, OsdKey::Next);
    advance(&mut osd, FAST_DETENT_MS);
    press(&mut osd, OsdKey::Next);
    assert_eq!(volume(&osd), 70);
    // Fast: 20 % a detent -- four of the volume's 5 % steps.
    advance(&mut osd, FAST_DETENT_MS - 40);
    press(&mut osd, OsdKey::Prev); // a new direction is never fast
    assert_eq!(volume(&osd), 65);
    advance(&mut osd, 40);
    press(&mut osd, OsdKey::Prev);
    assert_eq!(volume(&osd), 45);
    advance(&mut osd, 40);
    press(&mut osd, OsdKey::Prev);
    assert_eq!(volume(&osd), 25);
    // Arrows always step once, however fast.
    press(&mut osd, OsdKey::Down);
    press(&mut osd, OsdKey::Down);
    assert_eq!(volume(&osd), 15);
    press(&mut osd, OsdKey::Ok);
    // A stream's level: two of its 10 % steps.
    goto(&mut osd, AUDIO);
    pick_press(&mut osd, 7);
    press(&mut osd, OsdKey::Prev);
    press(&mut osd, OsdKey::Prev);
    assert_eq!(osd.host().settings.audio.music_level, 70);
    press(&mut osd, OsdKey::Ok);
    // The buffer and the lists stay one step a detent.
    pick_press(&mut osd, 5);
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Next);
    assert_eq!(osd.host().settings.audio.buffer_ms, 600);
    press(&mut osd, OsdKey::Ok);
    pick_press(&mut osd, 11);
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Next);
    assert_eq!(pick(&osd), (11, 1));
}

#[test]
fn night_picks_the_night_palette_and_auto_follows_the_phone_into_day() {
    let mut osd = osd(TestHost::new());
    let night = |osd: &Osd<TestHost>| {
        let b = osd.ui.global::<Bridge<'_>>();
        (b.get_night(), b.get_night_choice())
    };
    // Auto by default: the phone is told day, and so are the screens.
    assert_eq!(night(&osd), (false, 2));
    goto(&mut osd, SETTINGS);
    // A tap on Night: taken at once.
    osd.ui.global::<Bridge<'_>>().invoke_pick_press(2);
    osd.ui.global::<Bridge<'_>>().invoke_pick_option(1);
    tick(&mut osd);
    assert_eq!(night(&osd), (true, 1));
    assert!(osd.host().has(&Call::NightModeChanged));
    // The whole screen repaints in the other palette.
    let mut sink = Sink::default();
    osd.ui.global::<Bridge<'_>>().invoke_pick_press(2);
    osd.ui.global::<Bridge<'_>>().invoke_pick_option(0);
    assert!(osd.tick(false, &mut sink));
    assert_eq!(night(&osd), (false, 0));
    assert_eq!(sink.0, [(0, H)]);
    assert!(!pump::night_palette(lehu_core::settings::NightMode::Auto));
}

#[test]
fn the_home_tiles_follow_the_session_and_what_plays() {
    let mut osd = osd(TestHost::new());
    // The knob's round of the tiles, from the first one.
    let stops = |osd: &mut Osd<TestHost>| -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for _ in 0..8 {
            press(osd, OsdKey::Next);
            let at = ring(osd);
            if seen.contains(&at) {
                break;
            }
            seen.push(at);
        }
        let first = seen
            .iter()
            .position(|s| s == "resume" || s == "connect")
            .unwrap_or(0);
        seen.rotate_left(first);
        seen
    };
    // No phone: Connect (wide), Phones, Settings.
    assert_eq!(stops(&mut osd), ["connect", "phones", "settings"]);
    assert_eq!(
        prop!(osd, get_bt_name),
        "LEHU",
        "the Connect tile names the Bluetooth name"
    );
    // What plays sits beside it.
    playing(&mut osd);
    goto(&mut osd, SETTINGS);
    goto(&mut osd, HOME);
    assert_eq!(stops(&mut osd), ["connect", "media", "phones", "settings"]);
    // A session waiting: its tile in Connect's place, the Resume action.
    let host = osd.host_mut();
    host.status = "Android Auto: Pixel 8".into();
    host.backgrounded = true;
    tick(&mut osd);
    tick(&mut osd);
    assert_eq!(home_words(&osd).0, "Android Auto");
    assert_eq!(home_words(&osd).1, "Pixel 8");
    assert_eq!(stops(&mut osd), ["resume", "media", "phones", "settings"]);
    advance(&mut osd, KEY_RECENT_MS);
    goto(&mut osd, SETTINGS);
    goto(&mut osd, HOME);
    press(&mut osd, OsdKey::Next);
    assert_eq!(ring(&osd), "resume");
    press(&mut osd, OsdKey::Ok);
    assert!(osd.host().has(&Call::RequestResume));
    // Nothing plays: the phone's tile alone on its row.
    let host = osd.host_mut();
    host.media.available = false;
    host.media_gen += 1;
    tick(&mut osd);
    tick(&mut osd);
    assert_eq!(stops(&mut osd), ["resume", "phones", "settings"]);
    // The Phones tile opens the phones; Back goes Home again.
    advance(&mut osd, KEY_RECENT_MS);
    goto(&mut osd, SETTINGS);
    goto(&mut osd, HOME);
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Next);
    press(&mut osd, OsdKey::Ok);
    assert_eq!(screen(&osd), DEVICES);
    press(&mut osd, OsdKey::Back);
    assert_eq!((screen(&osd), ring(&osd)), (HOME, "phones".into()));
}

#[test]
fn the_clock_is_written_only_when_its_minute_changes() {
    let mut host = TestHost::new();
    host.local_time = Some((14 * 60 + 32) * 60_000 + 50_000); // 14:32:50
    let mut osd = osd(host);
    assert_eq!(prop!(osd, get_clock), "14:32");
    // The loop is woken for the next minute, ten seconds on.
    let due = osd.next_due().expect("the next minute");
    assert!((9_000..=10_000).contains(&due.as_millis()), "{due:?}");
    // Within the minute nothing is looked at or drawn.
    advance(&mut osd, 5_000);
    osd.host_mut().local_time = Some((14 * 60 + 32) * 60_000 + 55_000);
    let mut sink = Sink::default();
    assert!(!osd.tick(false, &mut sink));
    assert!(sink.0.is_empty());
    // The minute turns: written, and only the bar's band is drawn.
    advance(&mut osd, 5_000);
    osd.host_mut().local_time = Some((14 * 60 + 33) * 60_000);
    assert!(osd.tick(false, &mut sink));
    assert_eq!(prop!(osd, get_clock), "14:33");
    assert!(
        sink.0.iter().all(|&(y0, y1)| y1 <= 56 && y0 < y1),
        "{:?}",
        sink.0
    );
    // A clock that is not set shows nothing and wakes nobody.
    osd.host_mut().local_time = None;
    advance(&mut osd, 60_000);
    tick(&mut osd);
    assert_eq!(prop!(osd, get_clock), "");
    assert_eq!(osd.next_due(), None);
    // Early morning, as two digits.
    osd.host_mut().local_time = Some((7 * 60 + 5) * 60_000);
    advance(&mut osd, 10_000);
    tick(&mut osd);
    assert_eq!(prop!(osd, get_clock), "07:05");
}
