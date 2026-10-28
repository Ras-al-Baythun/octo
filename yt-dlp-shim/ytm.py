"""YouTube Music search and radio for Octo's radio, through ytmusicapi (no sign-in)."""
import http.client
import logging
import re
import threading

import requests
from ytmusicapi import YTMusic

log = logging.getLogger("ytshim.ytm")

# One client per thread: ytmusicapi promises no thread safety, and gunicorn serves the shim
# from 32 threads.
_local = threading.local()


def client():
    ytm = getattr(_local, "ytm", None)
    if ytm is None:
        ytm = _local.ytm = YTMusic()
    return ytm


def _fresh():
    """A new client for this thread, in place of one whose connection has gone stale."""
    _local.ytm = YTMusic()
    return _local.ytm


# A client that sat idle keeps a connection YouTube Music has closed meanwhile, and the next
# call fails at once ("Connection aborted", RemoteDisconnected) without a retry, since the
# calls are POSTs. One more go on a fresh client is what the retry would have been.
_STALE = (requests.exceptions.ConnectionError, ConnectionError, http.client.RemoteDisconnected)


def _call(what):
    """`what(client)`, once more on a fresh client when the connection was stale."""
    try:
        return what(client())
    except _STALE as e:
        log.info("ytm: stale connection (%s); trying once more on a fresh client", e)
        return what(_fresh())


def seconds(length):
    """'2:26' or '1:02:03' as seconds; None when it is not a length."""
    if not length or not isinstance(length, str):
        return None
    total = 0
    for part in length.split(":"):
        if not part.isdigit():
            return None
        total = total * 60 + int(part)
    return total


def _thumbnail(item):
    """The largest cover YouTube Music lists, asked of Google's image server at 544 px
    (its own sizes stop at 120 for a song row)."""
    pictures = [t for t in (item.get("thumbnails") or []) if t and t.get("url")]
    if not pictures:
        return None
    url = max(pictures, key=lambda t: t.get("width") or 0)["url"]
    return re.sub(r"=w\d+-h\d+", "=w544-h544", url)


def _row(item):
    video_id = item.get("videoId")
    title = item.get("title")
    if not video_id or not title:
        return None
    album = item.get("album")
    return {
        "videoId": video_id,
        "title": title,
        "artists": [a.get("name") for a in (item.get("artists") or []) if a and a.get("name")],
        "album": album.get("name") if isinstance(album, dict) else None,
        "durationSeconds": item.get("duration_seconds") or seconds(item.get("length") or item.get("duration")),
        "videoType": item.get("videoType") or ("MUSIC_VIDEO_TYPE_ATV" if item.get("resultType") == "song" else None),
        "thumbnail": _thumbnail(item),
    }


def search(query, filter_name="songs", limit=10):
    rows = _call(lambda ytm: ytm.search(query, filter=filter_name, limit=limit))
    return [r for r in (_row(item) for item in rows) if r]


def video_row(details):
    """One video as a row, from the player's videoDetails: any YouTube video, not only those
    YouTube Music lists, its channel as the artist."""
    video_id = (details or {}).get("videoId")
    title = (details or {}).get("title")
    if not video_id or not title:
        return None
    author = details.get("author") or ""
    length = details.get("lengthSeconds")
    return {
        "videoId": video_id,
        "title": title,
        "artists": [author.removesuffix(" - Topic")] if author else [],
        "album": None,
        "durationSeconds": int(length) if str(length or "").isdigit() else None,
        "videoType": details.get("musicVideoType") or "MUSIC_VIDEO_TYPE_UGC",
        "thumbnail": _thumbnail(details.get("thumbnail") or {}),
    }


def video(video_id):
    """The one video with this id, as a row (see video_row); [] when YouTube has none."""
    row = video_row(_call(lambda ytm: ytm.get_song(video_id)).get("videoDetails"))
    return [row] if row else []


def radio(video_id, limit=50):
    playlist = _call(lambda ytm: ytm.get_watch_playlist(videoId=video_id, radio=True, limit=limit))
    return [r for r in (_row(item) for item in playlist.get("tracks") or []) if r]


def artist_radio(name, limit=50):
    """The artist's radio, found through search: its rows carry the radio's playlist id
    (get_artist's radioId can be None on 1.12.3, ytmusicapi #1019). Returns (artist, rows)."""
    for hit in _call(lambda ytm: ytm.search(name, filter="artists", limit=5)):
        radio_id = hit.get("radioId")
        if radio_id:
            playlist = _call(lambda ytm: ytm.get_watch_playlist(playlistId=radio_id, limit=limit))
            return hit.get("artist"), [r for r in (_row(item) for item in playlist.get("tracks") or []) if r]
    return None, []
