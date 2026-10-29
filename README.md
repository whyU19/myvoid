## Install

Boot the Void live ISO (glibc), then:

```bash
git clone https://github.com/whyU19/myvoid && cd myvoid && sudo bash setup
```

`setup` asks for a mode:
1. **Full install**: wipes the whole disk and installs Void.
2. **Post install**: only installs packages, dotfiles, Rust tools and dwl.

Wayland only: **dwl** + **foot**. Log in on tty1 and dwl starts.

## Key bindings (Super)

| Key | Action |
|-----|--------|
| Shift+Enter / a | Terminal / launcher |
| Shift+q / Shift+l | Quit dwl / lock screen |
| j k / h l | Focus window / resize master |
| Shift+c | Kill window |
| t f m / Space | Layout: tile, floating, monocle / cycle |
| Shift+Space | Toggle floating |
| Shift+f or e | Fullscreen |
| 1-9 / Shift+1-9 / Tab | Switch tag / move window / previous tag |
| - = / Shift+- = / Shift+m | Brightness / volume / mute |
| x | Resolution and refresh rate |
| v | Clipboard history |
| w / Shift+b | Wi-Fi / Bluetooth |
| s / Shift+s | Screenshot full / area |
| r / Shift+r | Record screen / area (again to stop) |
| [ / ] | Previous / next background |

## Rust tools

Sources in `~/.config/tools`, binaries in `~/.config/<name>/<name>`.

| Tool | What it does |
|------|--------------|
| `menu` | launcher; `menu -r` is the launcher and calculator |
| `status` | Top bar and notification daemon |
| `setbg` | Background: `next`, `prev`, `restore`, or an image |
| `view` | Image viewer |
| `play` | Audio and video player (ffmpeg) |
| `display` | Resolution and refresh rate menu; `restore` |
| `lock` | Screen locker; `lock -a 300 600` locks after 300 s, screens off after 600 s |
| `capture` | `shot [area]`, `rec [area]` |
| `clipboard` | Copy, `paste`, `history`, `watch` |

Rebuild: `cd ~/.config/tools && cargo build --release`, then copy `target/release/<name>` to `~/.config/<name>/<name>`.

dwl: edit `~/.config/dwl/config.def.h`, then `rm config.h && make clean install`.
