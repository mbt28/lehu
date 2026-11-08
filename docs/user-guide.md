# User guide

LEHU's own screens sit under the phone's projection: you see them when no
phone is connected, and whenever you leave CarPlay or Android Auto with the
Menu or Home key.

LEHU can be used entirely with the touch panel, or entirely with a rotary
knob: every screen, setting, dialog and the on-screen keyboard can be
reached either way. Bluetooth keyboards and remotes work as well.

## Home

Four tiles:

- **The phone**: CarPlay or Android Auto, the phone's name and its link.
  Select it to go back to the phone. With no phone connected, this tile
  says how to connect one: plug it in, or pair it with the head unit's
  Bluetooth name.
- **Now playing**: the title, the artist and the progress. Opens the media
  page, with the album art and large previous, play/pause and next keys.
- **Phones**: the phones LEHU knows.
- **Settings**.

While the phone gives directions, a card at the top shows the next turn,
its distance, the road and the arrival time, over LEHU's screens and over
the camera.

## Settings

| Setting | What it does |
|---|---|
| Volume | the head unit's volume |
| Night | Day, Night or Auto, for the phone's projection; Night also dims LEHU's own screens |
| Phones | known phones: connect, stop or forget each one |
| Audio | levels for music, navigation, alerts and calls; the music buffer; the output device |
| Wireless | the access point (name, password, band, width, channel, country), the link speed, the Bluetooth name and Bluetooth devices |
| Video | Android Auto's frame rate, the reversing camera, its key and fast switch |
| Update | the version running; a new version of LEHU OS, brought by the phone |
| Power | reboot or power off |

Every change applies at once. Destructive actions (forget a phone, reboot,
power off) ask first and open on Cancel.

## Phones

Every link listens at the same time: wired and wireless CarPlay, wired and
wireless Android Auto. A phone on the cable always comes first. A phone
LEHU has seen before stays in the Phones list:

- **Connect** wakes it, over its cable or over Bluetooth, and ends the
  session of the phone that was connected.
- **Stop** ends its session and keeps it away until you connect it again.
- **Forget** removes it and its pairing.

For **wireless** use, pair the phone from its own Bluetooth settings with
the head unit's Bluetooth name (LEHU by default). The phone then joins the
head unit's Wi-Fi by itself.

## Controls

**Touch**: tap a tile or a row; the back arrow at the top left goes up a
level. On a level, the − and + keys change it; on a choice, tap the option.

**Rotary knob**: turn to move the amber ring from row to row, press to
select. A level or choice being changed follows the knob; press again to
finish. Turning fast moves volume and levels in larger steps.

**Keyboards and remotes**:

| Key | On LEHU's screens | On the phone |
|---|---|---|
| Arrows | move the ring | the phone's knob and navigation |
| Enter, OK | select | select |
| Esc, Back, Backspace | back | back |
| Home | Home | Home |
| Menu | Home; on Home, back to the phone | LEHU's screens |
| Volume, Mute | volume | volume |
| Play/Pause, Next, Previous | – | media control |
| Voice, Search | – | Siri or Google Assistant |
| Scroll wheel and its click | knob turn and press | the phone's knob |
| Letters and digits | typing in the editor | – |

The ring appears only after a key or the knob is used; touch never shows
it. When you come back to a page, the ring is on the row you left.

## Bluetooth keyboards and remotes

Settings › Wireless › Bluetooth › **Add device** searches for 30 seconds.
Select a found device to pair it: a keyboard asks you to type a code on it,
a remote pairs directly. Paired devices reconnect by themselves when they
wake. Open a device to connect, disconnect or forget it.

Searching is not possible while a wireless phone is connected, because the
phone and the search share one radio.

## Reversing camera

Settings › Video › **Camera** shows the composite camera full screen, with
a column for the input, the video standard and the picture levels (each
input keeps its own). A key learned under **Camera key** switches to the
camera and back from anywhere, including from the phone's video. **Fast
switch** keeps the phone's video ready behind the camera, where memory
allows, so the way back is instant.

## Updating LEHU OS

On LEHU OS, Settings › **Update** installs new versions. The head unit has
no internet of its own, so the phone fetches the update over its mobile
data and hands it over through the head unit's Wi-Fi. On other systems the
row says the update is not available there.

1. Settings › Update › **Get from phone** shows two QR codes.
2. Scan the first with the phone's camera: the phone joins the head unit's
   Wi-Fi (the name and password are under the code too). Keep the phone's
   mobile data on.
3. Scan the second: the update page opens in the phone's browser. It shows
   the new version and its size, or that the head unit is up to date.
4. Tap **Download and send to the head unit**. The phone downloads the
   update and sends it on; the head unit shows how much has arrived. If the
   transfer stops, try again: it goes on from where it stopped.
5. The head unit checks the update (its signature, and that it is made for
   the version running), then shows its version and how it installs.
6. When parked, select **Install** and confirm. A connected phone is
   disconnected first.
   - A new LEHU installs in place in about a minute, with its progress on
     the screen, then the head unit restarts.
   - An update of the OS restarts the head unit at once and installs while
     the start-up screen shows its progress, which takes about 4 minutes.
     Keep the power on.

   While it installs, Settings › Power is locked. If the head unit does
   not restart when it is done, the Update page says so and the phones can
   connect again; the update takes effect at the next start.

After the restart, the head unit says once whether the update went in. If
it was cut off (the power went), it says so, and the update can be
installed again from Settings › Update. A received update that was not
installed yet stays on the head unit until it is, or until another one
comes.

The phone only carries the update: the head unit checks it before it
installs anything. The page is served only from opening Get from phone
until an install starts, or ten minutes after the Update pages were left.

## Settings file

The settings live in a TOML file in LEHU's state directory, written a
moment after each change. The image can provide defaults in
`/etc/lehu/lehu.toml`; [config/lehu.toml](../config/lehu.toml) lists every
setting with its default.
