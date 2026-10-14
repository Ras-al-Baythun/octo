# Dashboard icons

`make_sprite.py` builds `octo/wwwroot/admin/icons.svg` from two sources.

## Phosphor

UI glyphs are [Phosphor Icons](https://phosphoricons.com) 2.1.1, the `@phosphor-icons/core`
package on npm, the version the Octo apps use. MIT licence, copied to `Phosphor-MIT.txt`.
Regular weight everywhere; Fill only for a state that is on or settled (a finished check, an
error). `icons.json` lists every glyph the dashboard uses.

## Brand marks

Each file in `brands/` is the service's own mark, sized for an icon. Nothing is redrawn except
where noted. The marks belong to their owners and are used only to name their service.

| File | From | Change |
| --- | --- | --- |
| `lastfm.svg` | [Simple Icons](https://simpleicons.org) 16.33.0 `lastdotfm` (CC0) | brand colour `#D51007` |
| `deezer.svg` | Simple Icons `deezer` | brand colour `#A238FF` |
| `discord.svg` | Simple Icons `discord` | brand colour `#5865F2` |
| `ntfy.svg` | Simple Icons `ntfy` | brand colour `#317F6F` |
| `youtube.svg` | Simple Icons `youtube` | brand red, plus the white play triangle YouTube's own mark has |
| `listenbrainz.svg` | [metabrainz/listenbrainz-server](https://github.com/metabrainz/listenbrainz-server) `frontend/img/listenbrainz_logo_icon.svg` | viewBox squared |
| `musicbrainz.svg` | metabrainz/listenbrainz-server `frontend/img/meb-icons/MusicBrainz.svg` | viewBox squared |
| `lidarr.svg` | [Lidarr/Lidarr](https://github.com/Lidarr/Lidarr) `Logo/Lidarr.svg` | the two 2px grey honeycomb textures removed: invisible at icon size, and 98% of the file |
| `acoustid.svg` | [acoustid/acoustid-server](https://github.com/acoustid/acoustid-server) `misc/favicon.svg` | editor markup removed |
| `navidrome.svg` | [navidrome/navidrome](https://github.com/navidrome/navidrome) `ui/public/safari-pinned-tab.svg` | that file is the outline only; the blue disc and white label of Navidrome's app icon (`ui/public/android-chrome-192x192.png`) are laid under it |

Octo's own logo is not in the sprite. It marks what came through Octo, so the dashboard shows it
once, beside the name.
