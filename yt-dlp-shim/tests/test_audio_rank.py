"""The top result is the music video as often as not. Among a few results, the song's own
audio (a label's "- Topic" channel, "Official Audio") plays first, its video last, and with a
hint nothing far from the hinted length beats what is near it."""
import json
import os
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

app_module = pytest.importorskip("app")


def ids(ranked):
    return [r["id"] for r in ranked]


def test_the_songs_own_audio_beats_its_video_of_the_same_length():
    candidates = [
        {"id": "video", "title": "Drake - Passionfruit (Official Video)", "duration": 299, "channel": "Drake"},
        {"id": "topic", "title": "Passionfruit", "duration": 298, "channel": "Drake - Topic"},
        {"id": "audio", "title": "Drake - Passionfruit (Official Audio)", "duration": 298, "channel": "OVO"},
    ]
    assert ids(app_module._rank(candidates, "Drake Passionfruit", 298)) == ["topic", "audio", "video"]
    assert ids(app_module._rank(candidates, "Drake Passionfruit", None)) == ["topic", "audio", "video"]


def test_with_a_hint_a_far_audio_upload_never_beats_the_near_video():
    candidates = [
        {"id": "video", "title": "Band - Song (Official Music Video)", "duration": 200, "channel": "Band"},
        {"id": "edit", "title": "Song", "duration": 420, "channel": "Band - Topic"},
    ]
    assert ids(app_module._rank(candidates, "Band Song", 201))[0] == "video"
    # Without a hint there is nothing to be far from: the audio comes first.
    assert ids(app_module._rank(candidates, "Band Song", None))[0] == "edit"


def test_live_still_comes_last_and_the_old_name_still_ranks():
    candidates = [
        {"id": "live", "title": "Song (Live)", "duration": 200, "channel": "Band - Topic"},
        {"id": "video", "title": "Band - Song (Official Video)", "duration": 201, "channel": "Band"},
    ]
    assert ids(app_module._rank_by_length(candidates, "Band Song", 200)) == ["video", "live"]


def test_without_a_hint_equal_ranks_keep_youtubes_order():
    candidates = [
        {"id": "b", "title": "Band - Song", "duration": 200, "channel": "someone"},
        {"id": "a", "title": "Band - Song (lyrics)", "duration": 200, "channel": "else"},
        {"id": "c", "title": "Band - Song HQ", "duration": 200, "channel": "other"},
    ]
    assert ids(app_module._rank(candidates, "Band Song", None)) == ["a", "b", "c"]


def _lines(rows):
    return "\n".join(json.dumps(r) for r in rows) + "\n"


@pytest.fixture
def client(monkeypatch):
    with app_module._SEARCH_CACHE_LOCK:
        app_module._SEARCH_CACHE.clear()
    return app_module.app.test_client()


def test_candidates_lists_ranked_rows_with_their_channels(client, monkeypatch):
    rows = [
        {"id": "video", "title": "Band - Song (Official Video)", "duration": 201, "channel": "Band"},
        {"id": "topic", "title": "Song", "duration": 200, "channel": "Band - Topic"},
    ]
    runs = []
    monkeypatch.setattr(app_module, "_run", lambda args, **kw: runs.append(args) or _lines(rows))
    answer = client.get("/candidates?q=Band+Song&n=6")
    assert answer.status_code == 200
    got = answer.get_json()["candidates"]
    assert [r["video_id"] for r in got] == ["topic", "video"]
    assert got[0]["channel"] == "Band - Topic"
    assert runs[0][0] == "ytsearch6:Band Song" and "--flat-playlist" in runs[0]
    # Served from the cache the second time.
    client.get("/candidates?q=Band+Song&n=6")
    assert len(runs) == 1


def test_search_plays_the_ranked_winner_and_resolves_it_once(client, monkeypatch):
    rows = [
        {"id": "video", "title": "Band - Song (Official Video)", "duration": 201, "channel": "Band"},
        {"id": "topic", "title": "Song", "duration": 200, "channel": "Band - Topic"},
    ]
    monkeypatch.setattr(app_module, "_run", lambda args, **kw: _lines(rows))
    resolved = []
    monkeypatch.setattr(app_module, "_resolve_url", lambda video_id, bg=False: resolved.append(video_id) or "http://x")
    monkeypatch.setattr(app_module, "_with_facts", lambda payload: payload)
    answer = client.get("/search?q=Band+Song")
    assert answer.status_code == 200
    assert answer.get_json()["video_id"] == "topic"
    assert resolved == ["topic"]
