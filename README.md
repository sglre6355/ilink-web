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

The video reaches the page over WebRTC, on the same port number over UDP, so
that it stays close to live even on slow or distant connections. `ilink-web`
encodes it as H.264 at the bitrate each viewer's connection can carry,
measured continuously, and sends gimbal commands over the same connection.
Encoding uses the GPU through VA-API when it is available and the OpenH264
software encoder otherwise. A page opened at `127.0.0.1` or `localhost` gets
the plain MJPEG stream instead, since browsers do not use WebRTC over the
loopback interface and locally it adds no delay.

`ilink-web` opens the camera when the first viewer connects and releases it
five seconds after the last one leaves, so the camera is free for video calls
when no one is watching. Only one program can take video from the camera at a
time, so the page shows no picture while another program has it; the gimbal
still moves.

### Over Tailscale

To reach it from your own devices and nothing else, listen on the machine's
Tailscale address and open `http://MACHINE-NAME:8081/` on the tailnet:

```sh
ilink-web --listen "$(tailscale ip -4):8081"
```

This is the fastest way to watch from elsewhere, as long as Tailscale
connects the devices directly; `tailscale status` says `direct` rather than
`relay` when it does.

### On the internet, through Cloudflare

To share it with people anywhere, without opening ports on your router,
Cloudflare can carry all of it: a Tunnel serves the page over HTTPS, Access
lets in only the people you name, and its TURN service relays the video.

1. **TURN key.** In the Cloudflare dashboard, under Realtime → TURN Server,
   create a TURN key and note its ID and API token.
2. **Tunnel.** Create a tunnel and give it a hostname:

   ```sh
   cloudflared tunnel login
   cloudflared tunnel create ilink
   cloudflared tunnel route dns ilink camera.example.com
   ```

3. **Access.** In Zero Trust → Access → Applications, add a self-hosted
   application for `camera.example.com` with a policy that allows only the
   email addresses of the people you share it with. They sign in with a
   one-time code sent to that address. Do not skip this: `ilink-web` has no
   login of its own, and without Access anyone with the address could watch
   and steer the camera.
4. **Run both:**

   ```sh
   export CLOUDFLARE_TURN_KEY_ID=...        # from step 1
   export CLOUDFLARE_TURN_KEY_API_TOKEN=...
   ilink-web --listen 127.0.0.1:8081 --udp 0.0.0.0:8081
   cloudflared tunnel run --url http://127.0.0.1:8081 ilink
   ```

Then send `https://camera.example.com/` to your friends.

The tunnel carries the page and the WebRTC setup but not the video, which is
UDP. For that, each viewer gets TURN credentials that `ilink-web` generates
from the key and that expire after a day; the API token itself never leaves
this machine. The viewer's browser connects to the nearest Cloudflare
location, and Cloudflare carries the video from there to this machine, which
learns its own public address from Cloudflare's STUN server so that the
video can come in through your router without a forwarded port. `--udp`
opens the UDP port on every interface, since loopback alone cannot reach the
internet.

No setup removes the time light takes: between London and Tokyo, for
example, every round trip takes about a quarter of a second, so the picture
trails by roughly that much and the camera answers a key press a little
later still.

Any other TURN server works too, with `--turn`, `--turn-user` and
`--turn-password` in place of the Cloudflare key; the user name and password
are then handed to every viewer as they are.

## Options

```
ilink-web [-l ADDR] [-u ADDR] [-d PATH] [-r WxH] [-f N] [--max-bitrate KBPS]
          [--stun HOST:PORT] [--turn URL --turn-user NAME --turn-password PASSWORD]
          [--cloudflare-turn-key ID --cloudflare-turn-token TOKEN]
```

- `-l, --listen ADDR` is where to serve the page. The default,
  `127.0.0.1:8081`, is reachable from this machine only.
- `-u, --udp ADDR` is where to take WebRTC traffic. The default is the
  `--listen` address, over UDP.
- `-d, --device PATH` selects a camera by device node. The default is the
  first Link camera found.
- `-r, --resolution WxH` is the video size to ask the camera for. The default
  is `1920x1080`; the Link 2 also offers `1280x720`, `1280x960`, `1920x1440`
  and `3840x2160`.
- `-f, --fps N` is how many frames per second to capture and send, from 1 to
  60. The default is 30.
- `--max-bitrate KBPS` caps the video bitrate for each viewer. The default is
  8000 (8 Mbit/s); the actual rate follows what the connection can take.
- `--stun HOST:PORT` is a STUN server, such as `stun.cloudflare.com:3478`.
  `ilink-web` asks it for this machine's public address and offers that to
  viewers, and the page uses it too.
- `--turn URL`, `--turn-user NAME` and `--turn-password PASSWORD` give a TURN
  server for viewers that cannot reach this machine directly. A `turn:` URL
  also serves as the STUN server when `--stun` is not given.
- `--cloudflare-turn-key ID` and `--cloudflare-turn-token TOKEN`, or the
  `CLOUDFLARE_TURN_KEY_ID` and `CLOUDFLARE_TURN_KEY_API_TOKEN` environment
  variables, use Cloudflare's TURN service instead: each viewer gets
  credentials generated from the key, and `stun.cloudflare.com` serves as the
  STUN server unless `--stun` is given. The environment variables keep the
  token out of the process list.

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
arriving. If they stop the page reconnects by itself, and it disconnects while
the tab is hidden so a phone in a pocket does not use data.

## Bandwidth

Over WebRTC each viewer gets as much as their connection carries, up to
`--max-bitrate`: the rate drops within moments when the network gets busy
and climbs back as it clears, so the picture gets softer rather than late.
1080p at 30 fps looks good from about 3 Mbit/s. A lower `--resolution` such
as `1280x720` looks sharper on slow links and takes less work to encode.

The MJPEG stream (`/stream`, and the page at `127.0.0.1`) sends the camera's
frames unchanged, around 300 to 500 KB each at 1080p, so at 30 fps it needs
roughly 100 Mbit/s. That suits this machine and a fast local network only.

## Security

There is no login. Anyone who can reach the page can watch the camera and
move it, so listen only on an address you trust, such as `127.0.0.1` or a
Tailscale address, or put Cloudflare Access (or another login) in front of it
as described above.

The UDP port only talks to browsers that have set up a session through the
page, and the video is encrypted. TURN credentials are handed to every
viewer, since their browsers log in to the TURN server: Cloudflare ones expire
after a day, but a `--turn-password` does not, so use a TURN account made for
this.

## HTTP API

The page uses these, and they are usable from scripts too.

- `GET /stream` is the live video as `multipart/x-mixed-replace` MJPEG, which
  browsers, `ffplay` and VLC can show directly.
- `GET /snapshot` is the current picture as one JPEG.
- `POST /webrtc` takes an SDP offer (`Content-Type: application/sdp`) and
  answers with `201 Created` and the SDP answer. The offer should ask to
  receive one video track, and may open a data channel for the commands
  below. `GET /webrtc` gives the `iceServers` to pass to
  `RTCPeerConnection`.

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

Over WebRTC the page sends the same paths, such as `/api/move?pan=10&tilt=0`,
as text messages on its data channel and gets the same JSON back. While idle
the server also sends the state every 1.5 seconds. Of several absolute moves
waiting their turn, only the newest is carried out.

## Building

Cargo fetches the library from GitHub. The VA-API encoder needs FFmpeg's
development files and libclang (on Debian and Ubuntu, `libavcodec-dev`,
`libavutil-dev`, `libclang-dev` and `pkg-config`):

```sh
git clone https://github.com/sglre6355/ilink-web
cd ilink-web
cargo build --release        # Rust 1.89 or later; `nix develop` provides one
./target/release/ilink-web --help
```

To build without FFmpeg, and encode with OpenH264 only, use
`cargo build --release --no-default-features`.

Your user needs read and write access to the camera's `/dev/video*` node,
and to `/dev/dri/renderD*` for the GPU encoder.

## License

MIT. See [LICENSE](LICENSE).
