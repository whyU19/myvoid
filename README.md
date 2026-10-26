## Quick Install (from Void live ISO)

### Boot Void live ISO (glibc)
```bash
git clone https://github.com/whyU19/myvoid && cd myvoid && sudo bash setup
```

`setup` asks for a mode first:
1. **Full install**: wipes the whole disk and installs Void.
2. **Post Install**: keeps your system and only installs the packages, dotfiles, Rust tools and dwl for an existing user (run it as root on the installed system).

**dwl** (compositor) + **foot** (terminal)
Log in on tty1 and dwl starts (`~/.bash_profile`).

### Key Bindings (dwl)

| Key | Action |
|-----|--------|
| Super+Shift+Enter | Open foot |
| Super+a | Launcher |
| Super+Shift+q | Quit dwl |
| Super+Shift+l | Lock screen (`lock`) |
| Super+n | File manager (`fm` in foot) |
| Super+j/k | Focus next/prev window |
| Super+h/l | Resize master/stack |
| Super+Shift+c | Kill window |
| Super+t / f / m | Tile / floating / monocle layout |
| Super+Space | Cycle layout |
| Super+Shift+Space | Toggle floating |
| Super+Shift+f or Super+e | Toggle fullscreen |
| Super+1-9 | Switch tag |
| Super+Shift+1-9 | Move window to tag |
| Super+Tab | Previous tag |
| Super+-/= | Decrease/increase brightness |
| Super+Shift+-/= | Volume down/up |
| Super+Shift+m | Mute |
| Super+x | `display`: menu with every resolution and refresh rate |
| Super+v | Clipboard history (with image preview) |
| Super+w / Super+Shift+b | Wi-Fi / Bluetooth menu |
| Super+s / Super+Shift+s | Screenshot full / area (`capture shot`) |
| Super+r / Super+Shift+r | Record screen / area (`capture rec`, press again to stop) |
| Super+[ / ] | Previous / next background (`setbg`, images in `~/Pictures/Backgrounds`) |

### Rust tools (`tools/`, cargo workspace)

| Tool | Protocols |
|------|----------|-----------|
| `menu` | wlr-layer-shell |
| `status` | bar and notification daemon | wlr-layer-shell, D-Bus |
| `setbg` | (`next`/`prev`/`restore`/IMAGE) | wlr-layer-shell |
| `view` | xdg-shell |
| `play` | (ffmpeg does the decoding) | xdg-shell |
| `display` | (menus, saved modes, `restore`) | wlr-output-management |
| `fm` | (terminal file manager with eza-style colours; preview with bat/eza; `e` edit; `fm -f/-m/-d OUT` is a file picker for other programs) | terminal only |
| `lock` | (`lock` now; `lock -a 300 600` locks after 300 s and turns screens off after 600 s; PAM service `lock`) | ext-session-lock, ext-idle-notify, wlr-output-power-management |
| `capture` | wlr-screencopy, wlr-layer-shell (needs ffmpeg for recording) |
| `clipboard` | (`clipboard history` = history menu; `clipboard watch` records the history) | wlr-data-control |

Sources are in `~/.config/tools`; rebuild with
`cd ~/.config/tools && cargo build --release` and copy
`target/release/<name>` to `~/.config/<name>/<name>`.

dwl sources are in `~/.config/dwl` (`make clean install` after editing `config.def.h`
and removing `config.h`).
