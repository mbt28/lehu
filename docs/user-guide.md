# User guide

LEHU's own screens sit under the phone's projection: you see them when no
phone is connected, and whenever you leave CarPlay or Android Auto with the
Menu or Home key.

LEHU can be used entirely with the touch panel, or entirely with a rotary
knob: every screen, setting, dialog and the on-screen keyboard can be
reached either way. Bluetooth keyboards and remotes work as well.

## Home

Five tiles:

- **The phone**: CarPlay or Android Auto, the phone's name and its link.
  Select it to go back to the phone. With no phone connected, this tile
  says how to connect one: plug it in, or pair it with the head unit's
  Bluetooth name.
- **Now playing**: the title, the artist and the progress. Opens the media
  page, with the album art and large previous, play/pause and next keys.
- **Linked Phones**: the phones LEHU knows.
- **Map**: the offline map from the SD card ([map.md](map.md)). The knob
  zooms and its click opens the map's menu (so does the bar's menu key):
  Go to, Home, Work, Route, Voice, and Stop route while a route is driven.
  Go to offers the recent destinations, one tap each, the nearest fuel,
  parking or charging, or a keyboard to type a street or a place, with the
  two best matches over it (List shows them all). A tap on one finds the
  route on the head unit and shows it whole for a few seconds, with Start
  and Cancel (the knob's click starts, Back cancels; left alone it starts
  by itself), then guides along it. Home and Work, once chosen, are one
  choice away. Route chooses the fastest or the shortest route, avoiding
  motorways or not, and warning of speed cameras or not.
- **Settings**.

While the phone gives directions, a card at the top shows the next turn,
its distance, the road and the arrival time, over LEHU's screens and over
the camera. While LEHU's own map guides along a route and the phone does
not, the card shows the map's next turn instead, also with the map closed.

## Settings

| Setting | What it holds |
|---|---|
| Connection | **Linked Phones**: known phones, to connect, stop or forget. **Wireless**: the access point (name, password, band, width, channel, country), the link speed, the Bluetooth name and Bluetooth devices |
| Audio | the volume; levels for music, navigation, alerts and calls; the music buffer; the output device |
| Video | Night (Day, Night or Auto, for the phone's projection; Night, and Auto once the sun is down, also dim LEHU's own screens); Android Auto's frame rate; the reversing camera, its key and fast switch |
| Map | **Region**: the map shown, from those on the card. **Voice**: the spoken prompts' voice. **Hardware cursor**: the position arrow on the display's hardware cursor, where it has one, and its **Cursor size** (Small, Medium or Large). **While turning**: how the map is made in a bend, Sharp, Soft colour (a little faster) or Half (much faster, pixelated) |
| System | **Update**: the version running, and a new version of the system brought by the phone. **Info**: the kernel, LEHU OS and LEHU versions, live memory and CPU use, the connection and its bitrate. **Mouse** and **Hide pointer**: a mouse as a pointer, and the pointer going when still. **Power**: reboot or power off |

Every change applies at once. Destructive actions (forget a phone, reboot,
power off) ask first and open on Cancel.

## Phones

Every link listens at the same time: wired and wireless CarPlay, wired and
wireless Android Auto. A phone on the cable always comes first. A phone
LEHU has seen before stays in the Linked Phones list:

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

**Mouse**: with Settings › System › **Mouse** on, a mouse, or a Bluetooth
keyboard's touchpad, moves a pointer over the screens and the phone's
picture. The left button touches where it points, so a drag drags. The
right button is Back. The wheel and its click stay the knob's. The pointer
is shown on the display's hardware cursor, so the rows appear only where
the display has one. With **Hide pointer** on, the pointer goes after 3
seconds without a move and comes back with the next. While the pointer
shows over the map, the position arrow leaves the hardware cursor for a
plane of its own, and returns when the pointer goes.

## Bluetooth keyboards and remotes

Settings › Connection › Wireless › Bluetooth › **Add device** searches for 30 seconds.
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

## Updating the system

On LEHU OS, and on the f1c200s Buildroot image, Settings › System ›
**Update** installs new versions. The head unit has
no internet of its own, so the phone fetches the update over its mobile
data and hands it over through the head unit's Wi-Fi. On other systems the
row says the update is not available there.

1. Settings › System › Update › **Get from phone** shows two QR codes.
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

   An install cannot be stopped, so the screen stays on its progress until
   the head unit restarts. If the head unit does not restart when it is
   done, the Update page says so and the phones can connect again; the
   update takes effect at the next start.

Leaving the Update page does not stop anything before **Install**: the
transfer and the check go on, and Settings › System shows how far it is. **Cancel
update**, on the Update page and beside the QR codes, stops a transfer and
removes what arrived.

After the restart, the head unit says once whether the update went in. If
it was cut off (the power went), it says so, and the update can be
installed again from Settings › System › Update. A received update that was not
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
