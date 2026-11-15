# ilink-web

A web page to watch and steer an Insta360 Link series webcam on Linux.

`ilink-web` serves a page with the live picture and moves the gimbal when you
press arrow keys, click the picture or use the on-screen pad. It talks to the
camera with the [insta360-link](https://github.com/sglre6355/insta360-link)
library, the same one the [ilink](https://github.com/sglre6355/ilink)
command-line tool uses, so both can be used at once.

This is an unofficial project and is not affiliated with Insta360. Only the
Link 2 (firmware 3.2.7) has been tested.

## Running it

```sh
ilink-web --listen 0.0.0.0:8081
```

Then open `http://HOST:8081/`. Video and controls come from the same server;
no other program is needed.

`ilink-web` sends the camera's own MJPEG frames as they are, without decoding
or re-encoding them. It opens the video stream when the first viewer connects
and releases it five seconds after the last one leaves, so the camera is free
for video calls when no one is watching. Only one program can take video from
the camera at a time, so the page shows no picture while another program has
it; the gimbal still moves.

### Over Tailscale

To reach it from your own devices and nothing else, listen on the machine's
Tailscale address and open `http://MACHINE-NAME:8081/` on the tailnet:

```sh
ilink-web --listen "$(tailscale ip -4):8081"
```

### Through a Cloudflare Tunnel

To share it on the internet, point a tunnel at a local address:

```sh
ilink-web --listen 127.0.0.1:8081
cloudflared tunnel run --url http://127.0.0.1:8081 NAME
```

Read the security note below first.

## Options

```
ilink-web [-l ADDR] [-d PATH] [-r WxH] [-f N]
```

- `-l, --listen ADDR` is where to serve the page. The default,
  `127.0.0.1:8081`, is reachable from this machine only.
- `-d, --device PATH` selects a camera by device node. The default is the
  first Link camera found.
- `-r, --resolution WxH` is the video size to ask the camera for. The default
  is `1920x1080`; the Link 2 also offers `1280x720`, `1280x960`, `1920x1440`
  and `3840x2160`.
- `-f, --fps N` is how many frames per second each viewer gets, from 1 to 60.
  The default is 15.

If the camera is missing at startup, or is unplugged later, `ilink-web` keeps
running and opens it again on the next request.

## Using the page

On a computer the picture fills the window and the controls stay hidden:

| Key | Does |
| --- | --- |
| Arrow keys | Pan and tilt by one step |
| Shift + arrow | Three steps |
| `1` `2` `3` `4` | Step of 1°, 5°, 15° or 30° |
| `+` `−`, mouse wheel | Zoom |
| Click on the picture | Turn to face that point |
| `C` | Recenter |
| `F` | Fullscreen |
| `H` | Show or hide the on-screen controls |

On a phone the controls sit below the picture: tap the picture to aim, hold an
arrow to keep moving, and drag the slider to zoom. Opening the page with
`#controls` on the end shows the controls on a computer too.

Steps get smaller as you zoom in. Aiming by click uses the Link 2's field of
view, so it lands close to the point rather than exactly on it.

The position shown in the corner follows the camera, including moves made by
tracking, gestures or `ilink`. The dot next to it is green while frames are
arriving. If they stop the page reconnects by itself, and it stops reading
the stream while the tab is hidden so a phone in a pocket does not use data.

## Bandwidth

The Link 2 sends MJPEG frames of around 300 to 400 KB at 1080p, and
`ilink-web` passes them through unchanged, so each viewer receives roughly
40 Mbit/s at 15 fps. That is fine on a local network. On mobile data, or when
several people watch through a tunnel on one upload link, lower `--fps` or
`--resolution`. A viewer whose connection cannot keep up gets fewer frames
rather than a growing delay.

## Security

There is no login. Anyone who can reach the page can watch the camera and
move it, so listen only on an address you trust, such as `127.0.0.1` or a
Tailscale address.

## HTTP API

The page uses these, and they are usable from scripts too.

- `GET /stream` is the live video as `multipart/x-mixed-replace` MJPEG, which
  browsers, `ffplay` and VLC can show directly.
- `GET /snapshot` is the current picture as one JPEG.

The rest return the camera state as JSON, or `{"error": "..."}` with a
non-200 status.

- `GET /api/state`
- `GET /api/move?pan=DEG&tilt=DEG&zoom=FACTOR` moves to an absolute
  position. `dpan`, `dtilt` and `dzoom` move relative to the current one.
  Any of them can be left out, and values are clamped to the camera's range.
- `GET /api/center` recenters the gimbal.

```console
$ curl -s 'http://127.0.0.1:8081/api/move?dpan=10'
{"pan":35,"tilt":-28,"zoom":1,"privacy":false,"limits":{"pan":[-145,145],"tilt":[-90,100],"zoom":[1,4]}}
```

The position in a reply is read straight after the command, so it can show
where the gimbal was rather than where it is heading.

## Building

Cargo fetches the library from GitHub:

```sh
git clone https://github.com/sglre6355/ilink-web
cd ilink-web
cargo build --release        # Rust 1.88 or later; `nix develop` provides one
./target/release/ilink-web --help
```

Your user needs read and write access to the camera's `/dev/video*` node.

## License

MIT. See [LICENSE](LICENSE).
