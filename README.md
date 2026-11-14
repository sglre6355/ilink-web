# ilink-web

A web page to watch and steer an Insta360 Link series webcam on Linux.

`ilink-web` serves a page with the live picture and moves the gimbal when you
press arrow keys, click the picture or use the on-screen pad. It talks to the
camera with the [insta360-link](https://github.com/sglre6355/insta360-link)
library, the same one the [ilink](https://github.com/sglre6355/ilink)
command-line tool uses, so both can be used at once.

This is an unofficial project and is not affiliated with Insta360. Only the
Link 2 (firmware 3.2.7) has been tested.

## How it fits together

`ilink-web` does not stream video itself. The picture comes from any program
that serves the camera as MJPEG over HTTP, such as
[ustreamer](https://github.com/pikvm/ustreamer), and the page reads it from
there:

```sh
ustreamer --device /dev/video0 --format MJPEG --resolution 1920x1080 \
  --desired-fps 15 --host 0.0.0.0 --port 8080 --allow-origin '*'
ilink-web --listen 0.0.0.0:8081
```

Then open `http://HOST:8081/`. The page fetches the stream from port 8080 on
the same host; `--stream` points it elsewhere. The stream server must send
`Access-Control-Allow-Origin` (ustreamer's `--allow-origin`), because the page
reads the stream with `fetch` so that it can reconnect.

Gimbal and zoom moves work whether or not anything is streaming.

### Over Tailscale

To reach it from your own devices and nothing else, listen on the machine's
Tailscale address:

```sh
ip=$(tailscale ip -4)
ustreamer --device /dev/video0 --format MJPEG --resolution 1920x1080 \
  --desired-fps 15 --host "$ip" --port 8080 --allow-origin '*'
ilink-web --listen "$ip:8081"
```

and open `http://MACHINE-NAME:8081/` on a phone or another computer on the
tailnet.

## Options

```
ilink-web [-l ADDR] [-d PATH] [-s URL]
```

- `-l, --listen ADDR` is where to serve the page. The default,
  `127.0.0.1:8081`, is reachable from this machine only.
- `-d, --device PATH` selects a camera by device node. The default is the
  first Link camera found.
- `-s, --stream URL` is the MJPEG stream to show. A value starting with `:`
  is a port and path on the host the page was loaded from; the default is
  `:8080/stream`.

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

The Link 2 sends MJPEG frames of around 300 to 400 KB at 1080p, and ustreamer
passes them through unchanged, so each viewer receives roughly 40 Mbit/s at
15 fps. That is fine on a local network; on mobile data, lower
`--desired-fps` or `--resolution`.

## Security

There is no login. Anyone who can reach the page can watch the camera and
move it, so listen only on an address you trust, such as `127.0.0.1` or a
Tailscale address.

## HTTP API

The page uses these, and they are usable from scripts too. Each returns the
camera state as JSON, or `{"error": "..."}` with a non-200 status.

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
