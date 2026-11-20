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
Encoding uses the GPU when it can, through Vulkan Video (AMD, Intel and NVIDIA
GPUs whose drivers support it) or else VA-API, and the OpenH264 software
encoder otherwise. A page opened at `127.0.0.1` or `localhost` gets
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

### On the internet

To share it with people anywhere, the page needs to be reachable over HTTPS
behind a login, and the video, which is UDP, needs a way in. Without a
forwarded port on your router, that way is a TURN server: each viewer's
browser sends the video through it, and `ilink-web` learns its own public
address from a STUN server so that the video can come in through the router.

`--ice-servers-command` gives the page its TURN servers. It runs a shell
command and passes on what it prints, an `{"iceServers": [...]}` object as
`RTCPeerConnection` takes it, to every page that connects for the next hour,
then runs it again. A TURN service's short-lived credentials can be fetched
that way, as long as they stay valid for more than an hour, and a fixed list
can come from `cat servers.json`. Credentials in it are handed to every
viewer, so they should expire or belong to an account made for this.

No setup removes the time light takes: between London and Tokyo, for
example, every round trip takes about a quarter of a second, so the picture
trails by roughly that much and the camera answers a key press a little
later still.

#### With Cloudflare

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
4. **Credentials.** Save this as `cloudflare-ice-servers.sh`. It asks
   Cloudflare for credentials that expire after a day, and leaves out the
   servers on port 53, which browsers do not use:

   ```sh
   #!/bin/sh
   curl -sf -X POST \
     -H "Authorization: Bearer $TURN_KEY_API_TOKEN" \
     -H 'Content-Type: application/json' -d '{"ttl": 86400}' \
     "https://rtc.live.cloudflare.com/v1/turn/keys/$TURN_KEY_ID/credentials/generate-ice-servers" |
     jq -c '.iceServers[].urls |= map(select(test(":53([?/]|$)") | not))'
   ```

5. **Run both:**

   ```sh
   export TURN_KEY_ID=... TURN_KEY_API_TOKEN=...    # from step 1
   ilink-web --listen 127.0.0.1:8081 --udp 0.0.0.0:8081 \
     --stun stun.cloudflare.com:3478 \
     --ice-servers-command 'sh cloudflare-ice-servers.sh'
   cloudflared tunnel run --url http://127.0.0.1:8081 ilink
   ```

Then send `https://camera.example.com/` to your friends.

The tunnel carries the page and the WebRTC setup, and Cloudflare's TURN
service the video, from the location nearest each viewer to this machine. The
API token stays on this machine. `--udp` opens the UDP port on every
interface, since loopback alone cannot reach the internet.

### In a container

The flake builds a container image that holds `ilink-web` and the libraries
it links, plus a shell, `curl` and `jq` for `--ice-servers-command`, and
nothing else: no distribution or package manager.

```sh
nix build .#image
podman load < result        # or: docker load < result
```

It needs the camera's device nodes, and the host's network so that WebRTC
offers the host's own addresses. It runs as an unprivileged user, so it also
needs permission to open the camera. With rootless Podman, `--userns keep-id`
and `--user` run it as you:

```sh
podman run --rm --network host --userns keep-id --user "$(id -u):$(id -g)" \
  --device /dev/video0 --device /dev/video1 \
  localhost/ilink-web --listen 0.0.0.0:8081
```

With Docker, give it the camera's group instead, by number since the image
has no group names: `--group-add "$(getent group video | cut -d: -f3)"`.
Options after the image name go to `ilink-web`. A script for
`--ice-servers-command` goes in with `-v`, and the variables it reads with
`-e`, such as `-v ./cloudflare-ice-servers.sh:/ice-servers.sh:ro -e
TURN_KEY_ID -e TURN_KEY_API_TOKEN` and then
`--ice-servers-command 'sh /ice-servers.sh'`.

The image carries no GPU drivers. It encodes on the GPU when the host's
drivers are passed in, and with OpenH264 on the CPU otherwise, where 1080p
may fall short of 30 fps on a slower machine; `-r 1280x720` keeps up.

On a machine with an NVIDIA GPU, the NVIDIA Container Toolkit passes in the
driver, and the video goes through Vulkan Video:

```sh
  --device nvidia.com/gpu=all    # Podman, after `nvidia-ctk cdi generate`
  --gpus all                     # Docker
```

On NixOS, with any GPU, an image built on the same machine can borrow the
host's drivers:

```sh
  --device /dev/dri -v /run/opengl-driver:/run/opengl-driver:ro -v /nix/store:/nix/store:ro
```

Mounting the host's `/nix/store` over the image's works because everything
in the image came from that same store; an image built on another machine
would not find its files.

## Options

```
ilink-web [-l ADDR] [-u ADDR] [-d PATH] [-r WxH] [-f N] [--max-bitrate KBPS]
          [--stun HOST:PORT] [--ice-servers-command COMMAND]
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
  viewers, and the page uses it too unless `--ice-servers-command` is given.
- `--ice-servers-command COMMAND` is a shell command that prints the page's
  `{"iceServers": [...]}` as JSON, for TURN servers; see
  [On the internet](#on-the-internet). Its output is reused for an hour. If
  it fails or takes over 10 seconds, the page gets the `--stun` server alone
  and the command runs again for the next page.

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
page, and the video is encrypted. TURN credentials from
`--ice-servers-command` are handed to every viewer, since their browsers log
in to the TURN server, so use ones that expire or an account made for this.

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
{"pan":35.0,"tilt":-28.0,"zoom":1.0,"privacy":false,"limits":{"pan":[-145.0,145.0],"tilt":[-90.0,100.0],"zoom":[1.0,4.0]}}
```

The position in a reply is read straight after the command, so it can show
where the gimbal was rather than where it is heading.

Over WebRTC the page sends the same paths, such as `/api/move?pan=10&tilt=0`,
as text messages on its data channel and gets the same JSON back. While idle
the server also sends the state every 1.5 seconds. Of several absolute moves
waiting their turn, only the newest is carried out.

## Building

Cargo fetches the library from GitHub. The GPU encoders need FFmpeg's
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
and to `/dev/dri/renderD*` (or the NVIDIA devices) for the GPU encoder. The
Vulkan encoder needs FFmpeg 7.1 or later at run time; with an older one,
VA-API or OpenH264 takes over.

## License

MIT. See [LICENSE](LICENSE).
