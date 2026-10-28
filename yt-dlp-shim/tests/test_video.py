"""One video by id, for a YouTube link pasted into a search, and the time a long video's
download is given."""
import os
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

ytm = pytest.importorskip("ytm")

DETAILS = {
    "videoId": "kJjDhKOZPQs",
    "title": "Ba Sing Se | Bustling City Ambience",
    "author": "Ambience Academy",
    "lengthSeconds": "4512",
    "thumbnail": {"thumbnails": [{"url": "https://i.ytimg.com/vi/kJjDhKOZPQs/hq.jpg", "width": 480}]},
}


def test_a_video_is_a_row_with_its_channel_as_the_artist():
    row = ytm.video_row(DETAILS)
    assert row["videoId"] == "kJjDhKOZPQs"
    assert row["artists"] == ["Ambience Academy"]
    assert row["durationSeconds"] == 4512
    assert row["videoType"] == "MUSIC_VIDEO_TYPE_UGC"
    assert row["thumbnail"] == "https://i.ytimg.com/vi/kJjDhKOZPQs/hq.jpg"


def test_a_topic_channel_is_named_for_its_artist_and_a_release_keeps_its_type():
    row = ytm.video_row({**DETAILS, "author": "Tom Odell - Topic", "musicVideoType": "MUSIC_VIDEO_TYPE_ATV"})
    assert row["artists"] == ["Tom Odell"]
    assert row["videoType"] == "MUSIC_VIDEO_TYPE_ATV"


def test_no_video_is_no_row():
    assert ytm.video_row(None) is None
    assert ytm.video_row({"videoId": "x"}) is None


app = pytest.importorskip("app")


def test_the_video_endpoint_wants_a_video_id(monkeypatch):
    monkeypatch.setattr(ytm, "video", lambda video_id: [ytm.video_row(DETAILS)])
    client = app.app.test_client()
    assert client.get("/ytm/video?id=not-an-id").status_code == 400
    answer = client.get("/ytm/video?id=kJjDhKOZPQs").get_json()
    assert answer["tracks"][0]["title"] == "Ba Sing Se | Bustling City Ambience"


def test_a_long_video_is_given_longer_to_download():
    assert app.download_timeout("") == 300
    assert app.download_timeout("60") == 300
    assert app.download_timeout("200") == 520
    assert app.download_timeout("4512") == 2 * 4512 + 120
    assert app.download_timeout("40000") == 2 * 3600
