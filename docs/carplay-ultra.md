# CarPlay Ultra

CarPlay Ultra is the newest CarPlay: the phone and the head unit
authenticate each other, and the phone can draw the car maker's look and
instruments.

## When it is used

LEHU offers Ultra by itself when its MFi coprocessor is version 3.0;
nothing has to be configured. With an older coprocessor, or with
`carplay.ultra = false` in the settings, CarPlay stays classic.

If a phone fails to start an Ultra session, LEHU offers classic CarPlay
from then on, until it is restarted. While Ultra runs, LEHU's screens name
the session "CarPlay Ultra".

## What LEHU provides

- The MFi 4 mutual authentication, with the pairing bound to it.
- The instrument panel that makes the phone choose Ultra. It has no
  picture of its own: the phone still sends one video stream, for the one
  display.
- The vehicle state, with no car data. The phone's vehicle settings show a
  LEHU section with its version.
- A car maker's theme, if one is installed (below).

Not provided, because they need car data or a second display: the
instrument cluster stream, climate, seats, fuel and range, tyres, the
radio, notifications.

## Themes

A theme is optional: it only changes the look. The phone downloads the
theme's artwork itself; the head unit needs just a small file naming it,
which is prepared on a computer with internet access:

```sh
scripts/ultra-theme.py list                       # the themes in Apple's catalog
scripts/ultra-theme.py make <asset> <version> <sdk>  # writes ultra-theme.txt
```

Copy `ultra-theme.txt` to the `carplay` directory in LEHU's state directory
(`/var/lib/lehu/carplay/` on a packaged image). It is read at each session
start.

## Checking it

With an Ultra-capable iPhone, the log shows, in order:

1. `carplay: ultra offered` (with or without a theme),
2. `carplay: mfi4: the phone and the head unit proved each other`,
3. `cp-av: GET /info, ... with the Ultra panels`,
4. `cp-av: SETUP vehicle state` and `the phone follows N values`.

If the phone falls back to classic CarPlay, the last `mfi4` or `cp-av`
line before it shows where.
