#!/usr/bin/env bash
# Builds the stub bodies made with ffmpeg, into parity/stubs/bodies/: two audio files a download
# stub can hand Octo (no corpus file uses them now), and the large cover the iTunes stub serves in
# 17 and 96.
# Check the output in, like the music library: encoder output differs between ffmpeg builds, and
# the stub advertises each file's exact size ({{size:bodies/...}}), Navidrome reports size and bit
# rate, and the tags Octo writes sit on top of these bytes. Re-record after regenerating.
#
#   neon-tide.flac    215 s, 11025 Hz mono sine, FLAC, about 0.9 MB. Tagged as a peer would tag
#                     it for ANOTHER release (an EP), with duplicate field names, so the C-group
#                     clean-up (2463c23) has something to remove. 11025 Hz keeps it small and makes
#                     the spectrum check answer Unknown (under 44.1 kHz), so it never holds it back.
#   paper-moons.m4a   244 s, 11025 Hz mono sine, AAC in MP4, about 0.5 MB. Served under the name
#                     "Paper Moons.flac" (85bba0f renames it to .m4a). Under 700000 bytes, so
#                     Find songs never picks it for a heart; only a pick fetches it.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$here/../stubs/bodies"
mkdir -p "$out"
ff() { ffmpeg -nostdin -hide_banner -loglevel error -y "$@"; }
bitexact=(-fflags +bitexact -flags:a +bitexact -map_metadata -1)

ff -f lavfi -i "sine=frequency=277:duration=215:sample_rate=11025" -ac 1 "${bitexact[@]}" \
   -c:a flac -compression_level 8 \
   -metadata TITLE="Neon Tide" -metadata ARTIST="Zephyr Echo" -metadata ALBUMARTIST="Zephyr Echo" \
   -metadata ALBUM="Harbor Lights EP" -metadata DATE=2018 -metadata YEAR=2018 \
   -metadata RELEASEDATE=2018-03-03 -metadata ORIGINALDATE=2018-03-03 \
   -metadata TRACKNUMBER=1 -metadata TRACKTOTAL=4 -metadata TOTALTRACKS=4 \
   -metadata DISCNUMBER=1 -metadata DISCTOTAL=1 -metadata GENRE="Dream Pop" -metadata ISRC=QZPAR1900002 \
   -metadata LABEL="Peer Records" -metadata ORGANIZATION="Peer Records Ltd" -metadata PUBLISHER="Peer Publishing" \
   -metadata BARCODE=0000000009999 -metadata UPC=0000000009999 -metadata CATALOGNUMBER=PEER-001 \
   -metadata MUSICBRAINZ_ALBUMID=11111111-1111-4111-8111-111111111111 \
   -metadata MUSICBRAINZ_RELEASEGROUPID=22222222-2222-4222-8222-222222222222 \
   -metadata ALBUMVERSION=Remastered -metadata COMPILATION=1 -metadata RELEASETYPE=ep \
   -metadata RELEASESTATUS=official -metadata RELEASECOUNTRY=GB \
   -metadata REPLAYGAIN_ALBUM_GAIN="-3.00 dB" -metadata COMMENT="ripped by alpha" \
   "$out/neon-tide.flac"

ff -f lavfi -i "sine=frequency=311:duration=244:sample_rate=11025" -ac 1 "${bitexact[@]}" \
   -c:a aac -b:a 16k -movflags +faststart \
   -metadata title="Paper Moons" -metadata artist="Zephyr Echo" -metadata album_artist="Zephyr Echo" \
   -metadata album="Moonlit Sessions" -metadata date=2017 -metadata track=5/9 -metadata disc=2/2 \
   -metadata genre="Lo-Fi" -metadata compilation=1 -metadata comment="ripped by lima" \
   "$out/paper-moons.m4a"

#   cover-aurora-1400.jpg   1400 px solid colour: a cover big enough to replace a soft one, and
#                     under the 1500 px embed limit, so it goes into the files as it is.
ff -f lavfi -i color=c=0x2a6f97:s=1400x1400:d=1 -frames:v 1 -fflags +bitexact -flags:v +bitexact -map_metadata -1    "$out/cover-aurora-1400.jpg"

touch -t 202001020304.05 "$out/neon-tide.flac" "$out/paper-moons.m4a" "$out/cover-aurora-1400.jpg"
ls -l "$out/neon-tide.flac" "$out/paper-moons.m4a" "$out/cover-aurora-1400.jpg"
( cd "$out" && sha256sum neon-tide.flac paper-moons.m4a cover-aurora-1400.jpg )
